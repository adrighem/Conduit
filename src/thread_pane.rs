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

use std::time::Instant;

use gettextrs::gettext;
use gtk::prelude::*;
use webkit6::prelude::WebViewExt;

use crate::message_html::{self, MessageHtmlContext};
use crate::models::SlackMessage;

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

    pub(crate) fn render(
        &self,
        channel_id: &str,
        messages: &[SlackMessage],
        context: &MessageHtmlContext,
        focus_message_ts: Option<&str>,
    ) {
        let title = gettext("Thread");
        self.title.set_title(&title);
        self.split.set_show_sidebar(true);
        if messages.is_empty() {
            self.load_html(&message_html::placeholder_document(
                &title,
                &gettext("No replies"),
            ));
            return;
        }

        let started = Instant::now();
        let html = message_html::conversation_document_with_focus(
            channel_id,
            messages,
            context,
            focus_message_ts,
        );
        log_performance(started, "html_generation", html.len());
        self.load_html(&html);
    }

    pub(crate) fn load_html(&self, html: &str) {
        let started = Instant::now();
        crate::debug::log("ui", &format!("load_thread_html bytes={}", html.len()));
        self.web_view
            .load_html(html, Some(message_html::base_uri()));
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
