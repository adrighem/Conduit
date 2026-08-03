/* thread_pane.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! GTK/WebKit presentation boundary for the open thread surface.
//!
//! Workspace state decides which thread is open and the window translates runtime events. This
//! type owns the visual lifecycle so those layers do not also need to coordinate the sidebar,
//! title, placeholder, and WebView as separate widgets.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use gettextrs::gettext;
use gtk::prelude::*;
use webkit6::prelude::WebViewExt;

use crate::message_html;

#[derive(Clone, Debug)]
pub(crate) struct ThreadPane {
    split: adw::OverlaySplitView,
    title: adw::WindowTitle,
    view_box: gtk::Box,
    web_view: Rc<RefCell<Option<webkit6::WebView>>>,
    web_view_creations: Rc<Cell<usize>>,
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
            web_view: Rc::new(RefCell::new(None)),
            web_view_creations: Rc::new(Cell::new(0)),
        }
    }

    pub(crate) fn has_web_view(&self) -> bool {
        self.web_view.borrow().is_some()
    }

    pub(crate) fn web_view_creation_count(&self) -> usize {
        self.web_view_creations.get()
    }

    pub(crate) fn web_view(&self) -> Option<webkit6::WebView> {
        self.web_view.borrow().clone()
    }

    pub(crate) fn attach_web_view(&self, web_view: webkit6::WebView) -> webkit6::WebView {
        if let Some(existing) = self.web_view() {
            return existing;
        }
        self.view_box.append(&web_view);
        self.web_view.replace(Some(web_view.clone()));
        self.web_view_creations
            .set(self.web_view_creations.get() + 1);
        web_view
    }

    pub(crate) fn is_open(&self) -> bool {
        self.split.shows_sidebar()
    }

    pub(crate) fn show_placeholder(&self, message: &str) {
        let title = gettext("Thread");
        self.title.set_title(&title);
        self.split.set_show_sidebar(true);
        self.load_html(&message_html::placeholder_document(&title, message));
    }

    pub(crate) fn close(&self) {
        self.split.set_show_sidebar(false);
        self.load_html(&message_html::placeholder_document(
            &gettext("Thread"),
            &gettext("No thread open"),
        ));
    }

    pub(crate) fn load_document(&self, html: &str) {
        self.title.set_title(&gettext("Thread"));
        self.split.set_show_sidebar(true);
        self.load_html(html);
    }

    pub(crate) fn load_html(&self, html: &str) {
        let Some(web_view) = self.web_view() else {
            return;
        };
        let started = Instant::now();
        crate::debug::log("ui", &format!("load_thread_html bytes={}", html.len()));
        web_view.load_html(html, Some(message_html::base_uri()));
        log_performance(started, "html_load_submit", html.len());
    }
}

fn log_performance(started: Instant, operation: &str, bytes: usize) {
    if crate::debug::enabled() {
        crate::debug::log(
            "performance",
            &format!(
                "{operation} surface=thread bytes={bytes} elapsed_ms={:.2}",
                started.elapsed().as_secs_f64() * 1_000.0
            ),
        );
    }
}
