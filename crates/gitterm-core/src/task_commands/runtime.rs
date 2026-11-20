//! Sole command ingress and runtime-owned reply storage, independent of a UI.

use super::ReplyId;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
pub type CommandResult = Result<Value, String>;

#[derive(Default)]
struct Replies {
    closed: bool,
    pending: HashMap<ReplyId, oneshot::Sender<CommandResult>>,
}

struct ReplyTicket {
    id: ReplyId,
    replies: Weak<Mutex<Replies>>,
}

impl Drop for ReplyTicket {
    fn drop(&mut self) {
        if let Some(replies) = self.replies.upgrade() {
            replies.lock().unwrap().pending.remove(&self.id);
        }
    }
}

/// Clones retain a deferred operation's reply, but not the application owner.
#[derive(Clone)]
pub struct CommandReply(Arc<ReplyTicket>);

impl std::fmt::Debug for CommandReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CommandReply").field(&self.id()).finish()
    }
}

impl CommandReply {
    pub fn id(&self) -> ReplyId {
        self.0.id
    }

    pub fn send(&self, result: CommandResult) {
        if let Some(replies) = self.0.replies.upgrade() {
            let sender = replies.lock().unwrap().pending.remove(&self.id());
            if let Some(sender) = sender {
                let _ = sender.send(result);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommandEnvelope<O> {
    pub operation: O,
    pub caller: Option<String>,
    pub reply: CommandReply,
}

pub struct CommandSender<O> {
    output: mpsc::UnboundedSender<CommandEnvelope<O>>,
    replies: Arc<Mutex<Replies>>,
    closed: CancellationToken,
}

impl<O> Clone for CommandSender<O> {
    fn clone(&self) -> Self {
        Self {
            output: self.output.clone(),
            replies: self.replies.clone(),
            closed: self.closed.clone(),
        }
    }
}

/// Dropping/cancelling the waiter releases reply storage, never accepted work.
pub struct PendingReply {
    id: ReplyId,
    replies: Arc<Mutex<Replies>>,
    response: oneshot::Receiver<CommandResult>,
}

impl Future for PendingReply {
    type Output = CommandResult;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.response).poll(cx).map(|result| {
            result.unwrap_or_else(|_| {
                Err("GitTerm closed the task command without a response".into())
            })
        })
    }
}

impl Drop for PendingReply {
    fn drop(&mut self) {
        self.replies.lock().unwrap().pending.remove(&self.id);
    }
}

impl<O> CommandSender<O> {
    pub fn enqueue(&self, operation: O, caller: Option<String>) -> Result<PendingReply, String> {
        let (sender, response) = oneshot::channel();
        let id = ReplyId(uuid::Uuid::new_v4());
        // Serialize admission with close, including the enqueue itself.
        let mut replies = self.replies.lock().unwrap();
        if replies.closed || self.output.is_closed() {
            return Err("GitTerm's task command bridge is unavailable".into());
        }
        replies.pending.insert(id, sender);
        let envelope = CommandEnvelope {
            operation,
            caller,
            reply: CommandReply(Arc::new(ReplyTicket {
                id,
                replies: Arc::downgrade(&self.replies),
            })),
        };
        if let Err(error) = self.output.send(envelope) {
            replies.pending.remove(&id);
            // Dropping the ticket also takes this lock.
            drop(replies);
            drop(error);
            return Err("GitTerm's task command bridge is unavailable".into());
        }
        Ok(PendingReply {
            id,
            replies: self.replies.clone(),
            response,
        })
    }

    pub async fn dispatch(&self, operation: O, caller: Option<String>) -> CommandResult {
        self.dispatch_with_timeout(operation, caller, COMMAND_TIMEOUT)
            .await
    }

    async fn dispatch_with_timeout(
        &self,
        operation: O,
        caller: Option<String>,
        timeout: Duration,
    ) -> CommandResult {
        tokio::time::timeout(timeout, self.enqueue(operation, caller)?)
            .await
            .map_err(|_| "GitTerm timed out while handling the task command".to_string())?
    }

    /// Owner shutdown releases waiters even if deferred reply clones survive.
    pub fn close(&self) {
        let mut replies = self.replies.lock().unwrap();
        replies.closed = true;
        replies.pending.clear();
        self.closed.cancel();
    }
}

pub struct CommandReceiver<O> {
    input: mpsc::UnboundedReceiver<CommandEnvelope<O>>,
    replies: Arc<Mutex<Replies>>,
    closed: CancellationToken,
}

impl<O> CommandReceiver<O> {
    pub async fn recv(&mut self) -> Option<CommandEnvelope<O>> {
        tokio::select! {
            biased;
            _ = self.closed.cancelled() => None,
            envelope = self.input.recv() => envelope,
        }
    }

    pub fn try_recv(&mut self) -> Result<CommandEnvelope<O>, mpsc::error::TryRecvError> {
        if self.closed.is_cancelled() {
            return Err(mpsc::error::TryRecvError::Disconnected);
        }
        self.input.try_recv()
    }
}

impl<O> Drop for CommandReceiver<O> {
    fn drop(&mut self) {
        let mut replies = self.replies.lock().unwrap();
        replies.closed = true;
        replies.pending.clear();
        self.closed.cancel();
    }
}

/// Retain at process scope; subscription rebuilds may take the receiver once.
pub struct CommandIngress<O> {
    sender: CommandSender<O>,
    receiver: Mutex<Option<CommandReceiver<O>>>,
}

impl<O> Default for CommandIngress<O> {
    fn default() -> Self {
        let (sender, receiver) = command_channel();
        Self {
            receiver: Mutex::new(Some(receiver)),
            sender,
        }
    }
}

/// A directly owned receiver for headless adapters and fixture dispatchers.
pub fn command_channel<O>() -> (CommandSender<O>, CommandReceiver<O>) {
    let (output, input) = mpsc::unbounded_channel();
    let replies = Arc::new(Mutex::new(Replies::default()));
    let closed = CancellationToken::new();
    (
        CommandSender {
            output,
            replies: replies.clone(),
            closed: closed.clone(),
        },
        CommandReceiver {
            input,
            replies,
            closed,
        },
    )
}

impl<O> CommandIngress<O> {
    pub fn sender(&self) -> CommandSender<O> {
        self.sender.clone()
    }

    pub fn take_receiver(&self) -> Option<CommandReceiver<O>> {
        self.receiver.lock().unwrap().take()
    }

    pub fn close(&self) {
        self.sender.close();
    }
}

impl<O> Drop for CommandIngress<O> {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn take_once_fifo_admission_allows_deferred_out_of_order_replies() {
        let ingress = CommandIngress::default();
        let sender = ingress.sender();
        let mut receiver = ingress.take_receiver().unwrap();
        assert!(ingress.take_receiver().is_none());
        let first = sender.enqueue(1, Some("first".into())).unwrap();
        let second = sender.enqueue(2, Some("second".into())).unwrap();
        let a = receiver.recv().await.unwrap();
        let b = receiver.recv().await.unwrap();
        assert_eq!((a.operation, b.operation), (1, 2));
        assert_eq!(a.caller.as_deref(), Some("first"));
        assert_ne!(a.reply.id(), b.reply.id());
        b.reply.send(Ok(json!(2)));
        assert_eq!(second.await.unwrap(), json!(2));
        assert_eq!(sender.replies.lock().unwrap().pending.len(), 1);
        a.reply.send(Ok(json!(1)));
        assert_eq!(first.await.unwrap(), json!(1));
    }

    #[tokio::test]
    async fn cloned_reply_race_is_at_most_once() {
        let (sender, mut receiver) = command_channel();
        let pending = sender.enqueue((), None).unwrap();
        let reply = receiver.recv().await.unwrap().reply;
        let other = reply.clone();
        let a = tokio::spawn(async move { reply.send(Ok(json!(1))) });
        let b = tokio::spawn(async move { other.send(Ok(json!(2))) });
        let (a, b) = tokio::join!(a, b);
        a.unwrap();
        b.unwrap();
        assert!([json!(1), json!(2)].contains(&pending.await.unwrap()));
        assert!(sender.replies.lock().unwrap().pending.is_empty());
    }

    #[tokio::test]
    async fn last_handle_drop_closes_response_but_intermediate_drop_does_not() {
        let (sender, mut receiver) = command_channel();
        let mut pending = Box::pin(sender.enqueue((), None).unwrap());
        let reply = receiver.recv().await.unwrap().reply;
        let clone = reply.clone();
        drop(reply);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(pending.as_mut().poll(cx).is_pending())).await
        );
        drop(clone);
        assert_eq!(
            pending.await.unwrap_err(),
            "GitTerm closed the task command without a response"
        );
        assert!(sender.replies.lock().unwrap().pending.is_empty());
    }

    #[tokio::test]
    async fn timeout_and_cancel_release_storage_without_retracting_accepted_commands() {
        let (sender, mut receiver) = command_channel();
        let result = sender
            .dispatch_with_timeout("timeout", None, Duration::from_millis(1))
            .await;
        assert_eq!(
            result.unwrap_err(),
            "GitTerm timed out while handling the task command"
        );
        assert!(sender.replies.lock().unwrap().pending.is_empty());
        let accepted = receiver.recv().await.unwrap();
        assert_eq!(accepted.operation, "timeout");
        // The owner can still finish accepted work; a late reply is harmless.
        accepted.reply.send(Ok(json!("saved after timeout")));
        let cancelled = sender.enqueue("cancelled", None).unwrap();
        drop(cancelled);
        assert!(sender.replies.lock().unwrap().pending.is_empty());
        assert_eq!(receiver.recv().await.unwrap().operation, "cancelled");
    }

    #[tokio::test]
    async fn owner_close_releases_pending_and_queued_replies_despite_surviving_handles() {
        let ingress = CommandIngress::default();
        let sender = ingress.sender();
        let mut receiver = ingress.take_receiver().unwrap();
        let pending = sender.enqueue((), None).unwrap();
        let reply = receiver.recv().await.unwrap().reply;
        let queued = sender.enqueue((), None).unwrap();
        drop(ingress);
        sender.close();
        assert_eq!(
            pending.await.unwrap_err(),
            "GitTerm closed the task command without a response"
        );
        assert!(queued.await.is_err());
        assert!(sender.enqueue((), None).is_err());
        assert!(receiver.recv().await.is_none());
        reply.send(Ok(json!("late")));
        assert!(sender.replies.lock().unwrap().pending.is_empty());
    }

    #[tokio::test]
    async fn receiver_drop_rejects_enqueue_and_releases_a_deferred_reply() {
        let (sender, mut receiver) = command_channel();
        let pending = sender.enqueue((), None).unwrap();
        let reply = receiver.recv().await.unwrap().reply;
        drop(receiver);
        assert_eq!(
            sender.enqueue((), None).err().unwrap(),
            "GitTerm's task command bridge is unavailable"
        );
        assert!(pending.await.is_err());
        reply.send(Ok(json!("late")));
        assert!(sender.replies.lock().unwrap().pending.is_empty());
    }

    #[tokio::test]
    async fn receiver_does_not_retain_its_sender() {
        let (sender, mut receiver) = command_channel::<()>();
        drop(sender);
        assert!(receiver.recv().await.is_none());
    }
}
