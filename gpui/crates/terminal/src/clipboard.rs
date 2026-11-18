#[cfg(not(test))]
pub use arboard::Clipboard;

#[cfg(test)]
pub use fixture::Clipboard;

#[cfg(test)]
mod fixture {
    use std::cell::RefCell;
    thread_local! { static TEXT: RefCell<String> = const { RefCell::new(String::new()) }; }
    pub struct Clipboard;
    impl Clipboard {
        pub fn new() -> Result<Self, ()> {
            Ok(Self)
        }
        pub fn get_text(&mut self) -> Result<String, ()> {
            Ok(TEXT.with(|t| t.borrow().clone()))
        }
        pub fn set_text(&mut self, text: String) -> Result<(), ()> {
            TEXT.with(|t| *t.borrow_mut() = text);
            Ok(())
        }
    }
}
