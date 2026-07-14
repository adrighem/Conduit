use std::collections::HashMap;
use std::fs;
use std::path::Path;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use gtk::gio;
use gtk::gio::prelude::*;
use gtk::glib;
use gtk::glib::variant::ToVariant;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::models::SlackConversation;
use crate::sidebar::{conversation_switcher_items_with_aliases, UserSearchAliases};
use crate::ConduitApplication;

const OBJECT_PATH: &str = "/eu/vanadrighem/conduit/SearchProvider";
const INTERFACE_XML: &str = r#"
<node>
  <interface name="org.gnome.Shell.SearchProvider2">
    <method name="GetInitialResultSet">
      <arg type="as" name="terms" direction="in"/>
      <arg type="as" name="results" direction="out"/>
    </method>
    <method name="GetSubsearchResultSet">
      <arg type="as" name="previous_results" direction="in"/>
      <arg type="as" name="terms" direction="in"/>
      <arg type="as" name="results" direction="out"/>
    </method>
    <method name="GetResultMetas">
      <arg type="as" name="results" direction="in"/>
      <arg type="aa{sv}" name="metas" direction="out"/>
    </method>
    <method name="ActivateResult">
      <arg type="s" name="result" direction="in"/>
      <arg type="as" name="terms" direction="in"/>
      <arg type="u" name="timestamp" direction="in"/>
    </method>
    <method name="LaunchSearch">
      <arg type="as" name="terms" direction="in"/>
      <arg type="u" name="timestamp" direction="in"/>
    </method>
  </interface>
</node>
"#;

#[derive(Debug, Deserialize)]
struct CachedSearchState {
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    conversations: Vec<SlackConversation>,
    #[serde(default)]
    user_names: HashMap<String, String>,
    #[serde(default)]
    user_search_aliases: UserSearchAliases,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ResultTarget {
    workspace_id: String,
    channel_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SearchResult {
    id: String,
    name: String,
    description: &'static str,
    icon_name: &'static str,
}

pub(crate) fn register(
    connection: &gio::DBusConnection,
    application: &ConduitApplication,
) -> Result<gio::RegistrationId, glib::Error> {
    let interface = gio::DBusNodeInfo::for_xml(INTERFACE_XML)?
        .lookup_interface("org.gnome.Shell.SearchProvider2")
        .expect("search provider interface missing from introspection XML");
    let application = application.downgrade();

    connection
        .register_object(OBJECT_PATH, &interface)
        .method_call(
            move |_connection, _sender, _path, _interface, method, parameters, call| {
                let Some(application) = application.upgrade() else {
                    call.return_dbus_error(
                        "eu.vanadrighem.conduit.Unavailable",
                        "Conduit is shutting down",
                    );
                    return;
                };

                match method {
                    "GetInitialResultSet" => {
                        let terms = parameters.child_get::<Vec<String>>(0);
                        let ids = search(&config::state_cache_dir(), &terms)
                            .into_iter()
                            .map(|result| result.id)
                            .collect::<Vec<_>>();
                        call.return_value(Some(&(ids,).to_variant()));
                    }
                    "GetSubsearchResultSet" => {
                        let terms = parameters.child_get::<Vec<String>>(1);
                        let ids = search(&config::state_cache_dir(), &terms)
                            .into_iter()
                            .map(|result| result.id)
                            .collect::<Vec<_>>();
                        call.return_value(Some(&(ids,).to_variant()));
                    }
                    "GetResultMetas" => {
                        let ids = parameters.child_get::<Vec<String>>(0);
                        let metas = result_metas(&config::state_cache_dir(), &ids);
                        call.return_value(Some(&(metas,).to_variant()));
                    }
                    "ActivateResult" => {
                        let id = parameters.child_get::<String>(0);
                        if let Some(target) = decode_target(&id) {
                            application.activate_action(
                                "open-conversation",
                                Some(&(target.workspace_id, target.channel_id).to_variant()),
                            );
                        }
                        call.return_value(None);
                    }
                    "LaunchSearch" => {
                        application.activate();
                        call.return_value(None);
                    }
                    _ => call.return_dbus_error(
                        "org.freedesktop.DBus.Error.UnknownMethod",
                        "Unknown search-provider method",
                    ),
                }
            },
        )
        .build()
}

fn search(cache_dir: &Path, terms: &[String]) -> Vec<SearchResult> {
    let query = terms.join(" ");
    if query.trim().is_empty() {
        return Vec::new();
    }

    let mut per_workspace = cached_states(cache_dir)
        .into_iter()
        .filter(|state| !state.workspace_id.trim().is_empty())
        .map(|state| {
            let current_user_id = current_user_id(&state.workspace_id);
            conversation_switcher_items_with_aliases(
                &state.conversations,
                &state.user_names,
                current_user_id,
                &query,
                Some(&state.user_search_aliases),
                None,
            )
            .into_iter()
            .map(|row| SearchResult {
                id: encode_target(&ResultTarget {
                    workspace_id: state.workspace_id.clone(),
                    channel_id: row.id,
                }),
                name: row.title,
                description: row.kind.accessible_name(),
                icon_name: row.kind.icon_name(),
            })
            .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let mut results = Vec::new();
    while results.len() < 20 && per_workspace.iter().any(|items| !items.is_empty()) {
        for items in &mut per_workspace {
            if !items.is_empty() {
                results.push(items.remove(0));
                if results.len() == 20 {
                    break;
                }
            }
        }
    }
    results
}

fn result_metas(cache_dir: &Path, ids: &[String]) -> Vec<HashMap<String, glib::Variant>> {
    let results = cached_states(cache_dir)
        .into_iter()
        .flat_map(|state| {
            let current_user_id = current_user_id(&state.workspace_id).map(ToString::to_string);
            let workspace_id = state.workspace_id;
            let user_names = state.user_names;
            state
                .conversations
                .into_iter()
                .map(move |conversation| {
                    let kind = crate::sidebar::conversation_kind(&conversation);
                    let target = ResultTarget {
                        workspace_id: workspace_id.clone(),
                        channel_id: conversation.id.clone(),
                    };
                    (
                        encode_target(&target),
                        SearchResult {
                            id: encode_target(&target),
                            name: conversation
                                .display_name_with_users(&user_names, current_user_id.as_deref()),
                            description: kind.accessible_name(),
                            icon_name: kind.icon_name(),
                        },
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect::<HashMap<_, _>>();

    ids.iter()
        .filter_map(|id| results.get(id))
        .map(|result| {
            let icon = gio::ThemedIcon::new(result.icon_name);
            HashMap::from([
                ("id".to_string(), result.id.to_variant()),
                ("name".to_string(), result.name.to_variant()),
                ("description".to_string(), result.description.to_variant()),
                (
                    "gicon".to_string(),
                    icon.serialize().expect("themed icon serializes"),
                ),
            ])
        })
        .collect()
}

fn cached_states(cache_dir: &Path) -> Vec<CachedSearchState> {
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return Vec::new();
    };
    let mut paths = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| fs::read_to_string(path).ok())
        .filter_map(|contents| serde_json::from_str(&contents).ok())
        .collect()
}

fn current_user_id(workspace_id: &str) -> Option<&str> {
    workspace_id
        .rsplit_once(':')
        .map(|(_, user)| user)
        .filter(|user| user.starts_with('U') || user.starts_with('W'))
}

fn encode_target(target: &ResultTarget) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(target).expect("search result target serializes"))
}

fn decode_target(id: &str) -> Option<ResultTarget> {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(id).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_dir() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("conduit-search-provider-{nonce}"))
    }

    #[test]
    fn result_ids_round_trip_without_exposing_workspace_or_conversation_ids() {
        let target = ResultTarget {
            workspace_id: "T123:U123".into(),
            channel_id: "D456".into(),
        };
        let id = encode_target(&target);

        assert_eq!(decode_target(&id), Some(target));
        assert!(!id.contains("T123"));
        assert!(!id.contains("D456"));
    }

    #[test]
    fn searches_cached_channels_and_direct_messages_with_shared_matching() {
        let directory = temp_dir();
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("workspace.json"),
            serde_json::json!({
                "workspace_id": "T123:U0",
                "conversations": [
                    {"id": "C1", "name": "architecture", "is_channel": true},
                    {"id": "D1", "user": "U1", "is_im": true}
                ],
                "user_names": {"U1": "Žilvinas Kuusas"},
                "user_search_aliases": {"U1": ["zilvinas", "kuusas"]}
            })
            .to_string(),
        )
        .unwrap();

        let dm = search(&directory, &["Zilvinas".into(), "Kuu".into()]);
        assert_eq!(dm.len(), 1);
        assert_eq!(dm[0].name, "Žilvinas Kuusas");
        assert_eq!(dm[0].description, "Direct message");

        let channel = search(&directory, &["arch".into()]);
        assert_eq!(channel.len(), 1);
        assert_eq!(channel[0].name, "#architecture");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn ignores_empty_queries_and_unidentified_legacy_caches() {
        let directory = temp_dir();
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("legacy.json"),
            r#"{"conversations":[{"id":"C1","name":"general","is_channel":true}]}"#,
        )
        .unwrap();

        assert!(search(&directory, &[" ".into()]).is_empty());
        assert!(search(&directory, &["general".into()]).is_empty());
        let _ = fs::remove_dir_all(directory);
    }
}
