//! A wrapper that reports where its content is when the pointer enters it.
//!
//! Iced's `mouse_area` says *that* the pointer entered, not where the
//! hovered widget sits, so a hint placed from the pointer's x ends up at
//! whichever edge the pointer came in from. This reports the content's
//! bounds in window coordinates instead (TRU-192 bar hints).
use iced::advanced::layout;
use iced::advanced::mouse;
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Operation, Tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget};
use iced::{Element, Event, Length, Rectangle, Size, Vector};

pub struct HoverBounds<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    on_enter: Box<dyn Fn(Rectangle) -> Message + 'a>,
    on_exit: Message,
}

pub fn hover_bounds<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    on_enter: impl Fn(Rectangle) -> Message + 'a,
    on_exit: Message,
) -> HoverBounds<'a, Message, Theme, Renderer> {
    HoverBounds {
        content: content.into(),
        on_enter: Box::new(on_enter),
        on_exit,
    }
}

#[derive(Default)]
struct State {
    hovered: bool,
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for HoverBounds<'_, Message, Theme, Renderer>
where
    Renderer: renderer::Renderer,
    Message: Clone,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        // Like mouse_area, track hover on every event, not just pointer
        // moves, so a layout change under a still pointer is seen too.
        let state = tree.state.downcast_mut::<State>();
        let bounds = layout.bounds();
        let hovered = cursor.is_over(bounds);
        if hovered != state.hovered {
            state.hovered = hovered;
            if hovered {
                shell.publish((self.on_enter)(bounds));
            } else {
                shell.publish(self.on_exit.clone());
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message, Theme, Renderer> From<HoverBounds<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a + Clone,
    Theme: 'a,
    Renderer: 'a + renderer::Renderer,
{
    fn from(widget: HoverBounds<'a, Message, Theme, Renderer>) -> Self {
        Element::new(widget)
    }
}
