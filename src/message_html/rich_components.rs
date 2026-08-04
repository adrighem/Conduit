use gettextrs::gettext;

use super::rich_model::{
    RichAccessory, RichAttachment, RichControl, RichField, RichImage, RichInline, RichInlineStyle,
    RichLinkedText, RichNode, RichTextNode,
};
use super::rich_plan::{plan_control, ControlPlan, RichRenderPlan};
use super::MessageHtmlContext;

pub(super) fn render(plan: &RichRenderPlan, context: &MessageHtmlContext) -> String {
    plan.document
        .nodes
        .iter()
        .map(|node| render_node(node, plan, context))
        .collect()
}

fn render_node(node: &RichNode, plan: &RichRenderPlan, context: &MessageHtmlContext) -> String {
    match node {
        RichNode::Text(text) => super::text_block_html(text, None, context),
        RichNode::Control(control) => render_control(control, plan, context),
        RichNode::Header(text) => format!(
            "<h3 class=\"block-header\">{}</h3>",
            super::escape_html(text)
        ),
        RichNode::Section {
            text,
            fields,
            accessory,
        } => {
            let mut html = text
                .as_deref()
                .map(|text| super::text_block_html(text, None, context))
                .unwrap_or_default();
            if !fields.is_empty() {
                let fields = fields
                    .iter()
                    .map(|field| super::text_block_html(field, Some("block-field"), context))
                    .collect::<String>();
                html.push_str(&format!("<div class=\"block-fields\">{fields}</div>"));
            }
            if let Some(accessory) = accessory {
                let accessory = match accessory {
                    RichAccessory::Control(control) => render_control(control, plan, context),
                    RichAccessory::Image(image) => render_image(image, context),
                };
                html.push_str(&format!("<div class=\"block-accessory\">{accessory}</div>"));
            }
            html
        }
        RichNode::Context(elements) => {
            super::text_block_html(&elements.join("  "), Some("context-block"), context)
        }
        RichNode::Divider => "<hr class=\"divider\">".to_string(),
        RichNode::Image(image) => render_image(image, context),
        RichNode::Actions(controls) => {
            let controls = controls
                .iter()
                .map(|control| render_control(control, plan, context))
                .collect::<String>();
            format!("<div class=\"block-actions\">{controls}</div>")
        }
        RichNode::RichText(nodes) => nodes
            .iter()
            .map(|node| render_rich_text_node(node, context))
            .collect(),
        RichNode::Attachment(attachment) => render_attachment(attachment, plan, context),
        RichNode::Unsupported { fallback, .. } => fallback
            .as_deref()
            .map(|text| super::text_block_html(text, Some("unsupported-block"), context))
            .unwrap_or_default(),
    }
}

fn render_image(image: &RichImage, context: &MessageHtmlContext) -> String {
    match image.url.as_deref().filter(|url| super::is_http_url(url)) {
        Some(url) => super::image_figure_html(
            url,
            Some(url),
            &image.alt,
            image.title.as_deref().or(Some(&gettext("Slack image"))),
            context,
        ),
        None => format!(
            "<p class=\"image-alt\">{}</p>",
            super::escape_html(
                &gettext("Image: {description}").replace("{description}", &image.alt)
            )
        ),
    }
}

fn render_control(
    control: &RichControl,
    plan: &RichRenderPlan,
    context: &MessageHtmlContext,
) -> String {
    let label_html = super::mrkdwn_to_html(&control.label, context);
    match plan_control(
        &control.label,
        control.url.as_deref(),
        control.confirmation_required,
    ) {
        ControlPlan::Navigate { url, .. } => format!(
            "<a class=\"block-action\" href=\"{}\" rel=\"noreferrer noopener\">{label_html}</a>",
            super::escape_html(&url)
        ),
        ControlPlan::Unavailable { .. } => format!(
            "<span class=\"block-action is-unavailable\" aria-disabled=\"true\">{label_html}</span>"
        ),
        ControlPlan::SlackHandoff { label } => {
            let accessible =
                gettext("Open this message in Slack to use {label}").replace("{label}", &label);
            match plan.control_handle.as_ref() {
                Some(handle) => format!(
                    "<a class=\"block-action is-external\" href=\"{}\" aria-label=\"{}\"><span class=\"control-label\">{label_html}</span><span class=\"slack-handoff\">{}</span></a>",
                    super::escape_html(&super::message_control_action_url(handle)),
                    super::escape_html(&accessible),
                    super::escape_html(&gettext("Open in Slack"))
                ),
                _ => format!(
                    "<span class=\"block-action is-unavailable\" aria-disabled=\"true\" title=\"{}\">{label_html}</span>",
                    super::escape_html(&accessible)
                ),
            }
        }
    }
}

fn render_rich_text_node(node: &RichTextNode, context: &MessageHtmlContext) -> String {
    match node {
        RichTextNode::Paragraph(inlines) => {
            format!("<p>{}</p>", render_inlines(inlines, context))
        }
        RichTextNode::Preformatted(inlines) => {
            format!(
                "<pre><code>{}</code></pre>",
                render_inlines(inlines, context)
            )
        }
        RichTextNode::Quote(inlines) => format!(
            "<blockquote class=\"rich-text-quote\">{}</blockquote>",
            render_inlines(inlines, context)
        ),
        RichTextNode::List { ordered, items } => {
            let tag = if *ordered { "ol" } else { "ul" };
            let items = items
                .iter()
                .map(|item| format!("<li>{}</li>", render_inlines(item, context)))
                .collect::<String>();
            format!("<{tag} class=\"rich-text-list\">{items}</{tag}>")
        }
    }
}

fn render_inlines(inlines: &[RichInline], context: &MessageHtmlContext) -> String {
    inlines
        .iter()
        .map(|inline| render_inline(inline, context))
        .collect()
}

fn render_inline(inline: &RichInline, context: &MessageHtmlContext) -> String {
    let (mut html, style) = match inline {
        RichInline::Text { text, style } => (escape_inline_html(text), *style),
        RichInline::Link { url, label, style } => {
            let html = if super::is_http_url(url) {
                format!(
                    "<a href=\"{}\" rel=\"noreferrer noopener\">{}</a>",
                    super::escape_html(url),
                    escape_inline_html(label)
                )
            } else {
                escape_inline_html(label)
            };
            (html, *style)
        }
        RichInline::User(user_id) => {
            let name = context
                .user_names
                .get(user_id)
                .map(String::as_str)
                .unwrap_or(user_id);
            let tooltip = context
                .user_full_names
                .get(user_id)
                .map(String::as_str)
                .unwrap_or(name);
            return super::mention_actions_html(user_id, name, tooltip);
        }
        RichInline::Channel(channel_id) => {
            let name = context
                .conversation_titles
                .get(channel_id)
                .map(String::as_str)
                .unwrap_or(channel_id);
            return format!(
                "<a class=\"channel-reference\" href=\"{}\">#{}</a>",
                super::escape_html(&super::channel_action_url(channel_id)),
                super::escape_html(name)
            );
        }
        RichInline::Emoji(name) => {
            return super::mrkdwn_to_html(&format!(":{name}:"), context);
        }
    };
    apply_style(&mut html, style);
    html
}

fn escape_inline_html(text: &str) -> String {
    let normalized = text
        .replace("\r\n", "\n")
        .replace(['\r', '\u{2028}', '\u{2029}'], "\n");
    super::escape_html(&normalized).replace('\n', "<br>")
}

fn apply_style(html: &mut String, style: RichInlineStyle) {
    if style.code {
        *html = format!("<code>{html}</code>");
    }
    if style.bold {
        *html = format!("<strong>{html}</strong>");
    }
    if style.italic {
        *html = format!("<em>{html}</em>");
    }
    if style.strike {
        *html = format!("<s>{html}</s>");
    }
    if style.underline {
        *html = format!("<u>{html}</u>");
    }
}

fn render_attachment(
    attachment: &RichAttachment,
    plan: &RichRenderPlan,
    context: &MessageHtmlContext,
) -> String {
    let style = attachment
        .color
        .as_deref()
        .map(|color| {
            format!(
                " style=\"--attachment-accent:{}\"",
                super::escape_html(color)
            )
        })
        .unwrap_or_default();
    let mut content = String::new();
    for (text, class_name) in [
        (attachment.pretext.as_deref(), "attachment-pretext"),
        (attachment.text.as_deref(), "attachment-text"),
    ] {
        if let Some(text) = text {
            content.push_str(&super::text_block_html(text, Some(class_name), context));
        }
    }
    if let Some(author) = attachment.author.as_ref() {
        content.push_str(&render_linked_text("attachment-author", author));
    }
    if let Some(title) = attachment.title.as_ref() {
        content.push_str(&render_linked_text("attachment-title", title));
    }
    if !attachment.fields.is_empty() {
        let fields = attachment
            .fields
            .iter()
            .map(|field| render_field(field, context))
            .collect::<String>();
        content.push_str(&format!("<div class=\"attachment-fields\">{fields}</div>"));
    }
    if content.is_empty() {
        if let Some(fallback) = attachment.fallback.as_deref() {
            content.push_str(&super::text_block_html(
                fallback,
                Some("attachment-fallback"),
                context,
            ));
        }
    }
    if let Some(image) = attachment.image.as_ref() {
        content.push_str(&render_image(image, context));
    }
    if !attachment.actions.is_empty() {
        let actions = attachment
            .actions
            .iter()
            .map(|action| render_control(action, plan, context))
            .collect::<String>();
        content.push_str(&format!("<div class=\"block-actions\">{actions}</div>"));
    }
    if let Some(footer) = attachment.footer.as_deref() {
        content.push_str(&format!(
            "<p class=\"attachment-footer\">{}</p>",
            super::escape_html(footer)
        ));
    }
    format!("<section class=\"legacy-attachment\"{style}>{content}</section>")
}

fn render_linked_text(class_name: &str, linked: &RichLinkedText) -> String {
    let label = super::escape_html(&linked.text);
    let label = linked
        .url
        .as_deref()
        .filter(|url| super::is_http_url(url))
        .map(|url| {
            format!(
                "<a href=\"{}\" rel=\"noreferrer noopener\">{label}</a>",
                super::escape_html(url)
            )
        })
        .unwrap_or(label);
    format!("<p class=\"{class_name}\">{label}</p>")
}

fn render_field(field: &RichField, context: &MessageHtmlContext) -> String {
    let short_class = if field.short { " is-short" } else { "" };
    match (field.title.as_deref(), field.value.as_deref()) {
        (Some(title), Some(value)) => format!(
            "<div class=\"attachment-field{short_class}\"><strong>{}</strong>{}</div>",
            super::escape_html(title),
            super::text_block_html(value, None, context)
        ),
        (Some(title), None) => format!(
            "<div class=\"attachment-field{short_class}\"><strong>{}</strong></div>",
            super::escape_html(title)
        ),
        (None, Some(value)) => format!(
            "<div class=\"attachment-field{short_class}\">{}</div>",
            super::text_block_html(value, None, context)
        ),
        (None, None) => String::new(),
    }
}
