/* thread_pane.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! GTK geometry boundary for the open thread surface.
//!
//! Workspace state decides which thread is open and the window owns timeline document and delta
//! presentation. This type only coordinates the split view, title, and WebView placement.

use gettextrs::gettext;
use gtk::prelude::*;

#[derive(Clone, Debug)]
pub(crate) struct ThreadPane {
    split: adw::OverlaySplitView,
    title: adw::WindowTitle,
    web_view: webkit6::WebView,
}

impl ThreadPane {
    pub(crate) fn new(
        split: &adw::OverlaySplitView,
        title: &adw::WindowTitle,
        view_box: &gtk::Box,
        web_view: webkit6::WebView,
    ) -> Self {
        view_box.append(&web_view);
        Self {
            split: split.clone(),
            title: title.clone(),
            web_view,
        }
    }

    pub(crate) fn web_view(&self) -> webkit6::WebView {
        self.web_view.clone()
    }

    pub(crate) fn is_open(&self) -> bool {
        self.split.shows_sidebar()
    }

    /// Reveal the thread pane while its document is loading or ready.
    pub(crate) fn show(&self) {
        self.title.set_title(&gettext("Thread"));
        self.split.set_show_sidebar(true);
    }

    pub(crate) fn close(&self) {
        self.title.set_title(&gettext("Thread"));
        self.split.set_show_sidebar(false);
    }
}
