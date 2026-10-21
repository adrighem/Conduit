use crate::models::SlackMessage;

pub(crate) fn bob_message() -> SlackMessage {
    serde_json::from_value(serde_json::json!({
        "ts": "1710000000.000100",
        "bot_profile": {
            "name": "People assistant",
            "icons": { "image_72": "https://cdn.example.test/bot.png" }
        },
        "attachments": [{
            "color": "good",
            "pretext": "A request needs review",
            "title": "Review request",
            "text": "Choose an outcome",
            "fields": [{ "title": "Employee", "value": "Example Person", "short": true }],
            "image_url": "https://cdn.example.test/request.png",
            "callback_id": "private-callback",
            "actions": [{
                "type": "button",
                "text": "Approve",
                "name": "decision",
                "value": "private-approval-value"
            }]
        }]
    }))
    .expect("synthetic Bob message is valid")
}

pub(crate) fn jira_message() -> SlackMessage {
    serde_json::from_value(serde_json::json!({
        "ts": "1710000000.000100",
        "blocks": [
            {
                "type": "header",
                "text": { "type": "plain_text", "text": "Issue updated" }
            },
            {
                "type": "rich_text",
                "elements": [{
                    "type": "rich_text_section",
                    "elements": [
                        { "type": "text", "text": "Status: ", "style": { "bold": true } },
                        {
                            "type": "link",
                            "url": "https://issues.example.test/ABC-1",
                            "text": "ABC-1"
                        }
                    ]
                }]
            },
            {
                "type": "section",
                "fields": [
                    { "type": "mrkdwn", "text": "*Priority*\nHigh" },
                    { "type": "mrkdwn", "text": "*Owner*\nExample User" }
                ],
                "accessory": {
                    "type": "image",
                    "image_url": "https://cdn.example.test/issue.png",
                    "alt_text": "Issue icon"
                }
            },
            { "type": "future_widget", "private": "ignored" },
            {
                "type": "actions",
                "elements": [
                    { "type": "malformed_without_label", "value": "ignored-sibling-value" },
                    {
                        "type": "button",
                        "text": { "type": "plain_text", "text": "Open issue" },
                        "url": "https://issues.example.test/ABC-1"
                    },
                    {
                        "type": "button",
                        "text": { "type": "plain_text", "text": "Assign" },
                        "action_id": "private-action",
                        "value": "private-value"
                    },
                    {
                        "type": "static_select",
                        "placeholder": { "type": "plain_text", "text": "Set status" },
                        "action_id": "private-select",
                        "options": [{
                            "text": { "type": "plain_text", "text": "Done" },
                            "value": "private-option"
                        }]
                    },
                    {
                        "type": "overflow",
                        "action_id": "private-overflow",
                        "options": [{
                            "text": { "type": "plain_text", "text": "Delete" },
                            "value": "private-delete"
                        }]
                    }
                ]
            }
        ]
    }))
    .expect("synthetic Jira message is valid")
}
