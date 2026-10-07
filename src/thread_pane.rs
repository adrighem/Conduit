/* thread_pane.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! GTK presentation boundary for the open thread surface.
//!
//! Workspace state decides which thread is open and the window translates runtime events. This
//! type owns the visual lifecycle so those layers do not also need to coordinate the sidebar,
//! title, and placeholder as separate widgets.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gettextrs::gettext;
use gtk::prelude::*;

#[derive(Clone, Debug)]
pub(crate) struct ThreadPane {
    split: adw::OverlaySplitView,
    title: adw::WindowTitle,
    view_box: gtk::Box,
    pub(crate) native_timeline_view:
        Rc<RefCell<Option<crate::timeline_message_widget::NativeTimelineView>>>,
    native_timeline_creations: Rc<Cell<u32>>,
}

impl ThreadPane {
    pub(crate) fn new(
        split: &adw::OverlaySplitView,
        title: &adw::WindowTitle,
        view_box: &gtk::Box,
    ) -> Self {
        Self {
            split: split.clone(),
            title: title.clone(),
            view_box: view_box.clone(),
            native_timeline_view: Rc::new(RefCell::new(None)),
            native_timeline_creations: Rc::new(Cell::new(0)),
        }
    }

    pub(crate) fn ensure_native_timeline(
        &self,
    ) -> crate::timeline_message_widget::NativeTimelineView {
        if let Some(view) = self.native_timeline_view.borrow().as_ref().cloned() {
            return view;
        }
        let view = crate::timeline_message_widget::NativeTimelineView::new();
        self.view_box.append(view.widget());
        self.native_timeline_view.replace(Some(view.clone()));
        self.native_timeline_creations
            .set(self.native_timeline_creations.get() + 1);
        view
    }

    pub(crate) fn has_native_timeline(&self) -> bool {
        self.native_timeline_view.borrow().is_some()
    }

    /// Number of thread timelines built; the pane creates one lazily and then reuses it.
    pub(crate) fn native_timeline_creations(&self) -> u32 {
        self.native_timeline_creations.get()
    }

    pub(crate) fn is_open(&self) -> bool {
        self.split.shows_sidebar()
    }

    pub(crate) fn show_placeholder(&self, message: &str) {
        let title = gettext("Thread");
        self.title.set_title(&title);
        self.split.set_show_sidebar(true);
        self.ensure_native_timeline().show_placeholder(message);
    }

    pub(crate) fn close(&self) {
        self.split.set_show_sidebar(false);
        if let Some(native_view) = self.native_timeline_view.borrow().as_ref() {
            native_view.set_messages(
                &[],
                &crate::message_html::MessageHtmlContext::default(),
                None,
            );
        }
    }

    pub(crate) fn ensure_open(&self) {
        self.title.set_title(&gettext("Thread"));
        self.split.set_show_sidebar(true);
    }
}
