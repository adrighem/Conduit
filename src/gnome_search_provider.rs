use std::collections::{HashMap, HashSet};
use std::path::Path;

#[cfg(test)]
use std::fs;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use gtk::gio;
use gtk::gio::prelude::*;
use gtk::glib;
use gtk::glib::variant::ToVariant;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::models::SlackConversation;
use crate::sidebar::{
    conversation_kind, conversation_switcher_items_with_aliases, ConversationKind,
    UserSearchAliases,
};
use crate::store::{self, CACHE_VERSION};
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

#[derive(Debug, Serialize, Deserialize)]
struct CachedSearchState {
    version: u32,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    conversations: Vec<SlackConversation>,
    #[serde(default)]
    user_names: HashMap<String, String>,
    #[serde(default)]
    user_full_names: HashMap<String, String>,
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
                        let previous_results = parameters.child_get::<Vec<String>>(0);
                        let terms = parameters.child_get::<Vec<String>>(1);
                        let ids = subsearch(&config::state_cache_dir(), &previous_results, &terms)
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
    search_states(cached_states(cache_dir), terms)
}

fn subsearch(cache_dir: &Path, _previous_ids: &[String], terms: &[String]) -> Vec<SearchResult> {
    // The broad previous query may have more matches than GNOME Shell asks us
    // to return. Re-rank the complete catalog so a more specific query can
    // surface an item that was outside the previous top 20.
    search(cache_dir, terms)
}

fn search_states(states: Vec<CachedSearchState>, terms: &[String]) -> Vec<SearchResult> {
    let query = terms.join(" ");
    if query.trim().is_empty() {
        return Vec::new();
    }

    let mut per_workspace = states
        .into_iter()
        .filter(|state| !state.workspace_id.trim().is_empty())
        .map(|mut state| {
            state.conversations.retain(|conversation| {
                !conversation.is_archived.unwrap_or(false)
                    && conversation_kind(conversation) != ConversationKind::Unknown
            });
            add_virtual_direct_messages(&mut state);
            let current_user_id = current_user_id(&state.workspace_id);

            conversation_switcher_items_with_aliases(
                &state.conversations,
                &state.user_names,
                current_user_id,
                &query,
                Some(&state.user_search_aliases),
                Some(&state.user_full_names),
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
        .flat_map(|mut state| {
            let current_user_id = current_user_id(&state.workspace_id).map(ToString::to_string);
            let workspace_id = state.workspace_id.clone();
            state.conversations.retain(|conversation| {
                !conversation.is_archived.unwrap_or(false)
                    && conversation_kind(conversation) != ConversationKind::Unknown
            });
            add_virtual_direct_messages(&mut state);

            let user_names = state.user_names;
            let user_full_names = state.user_full_names;

            state
                .conversations
                .into_iter()
                .map(move |conversation| {
                    let kind = conversation_kind(&conversation);
                    let target = ResultTarget {
                        workspace_id: workspace_id.clone(),
                        channel_id: conversation.id.clone(),
                    };
                    (
                        encode_target(&target),
                        SearchResult {
                            id: encode_target(&target),
                            name: conversation.navigation_name_with_users(
                                &user_names,
                                &user_full_names,
                                current_user_id.as_deref(),
                            ),
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
                    "icon".to_string(),
                    icon.serialize().expect("themed icon serializes"),
                ),
            ])
        })
        .collect()
}

fn add_virtual_direct_messages(state: &mut CachedSearchState) {
    let current_user_id = current_user_id(&state.workspace_id);
    let existing_im_users = state
        .conversations
        .iter()
        .filter(|conversation| conversation.is_im.unwrap_or(false))
        .filter_map(|conversation| conversation.user.as_ref().map(ToString::to_string))
        .collect::<HashSet<_>>();

    for user_id in state.user_names.keys() {
        if Some(user_id.as_str()) == current_user_id || existing_im_users.contains(user_id) {
            continue;
        }
        state.conversations.push(SlackConversation {
            id: user_id.clone(),
            name: None,
            user: Some(user_id.clone()),
            is_channel: Some(false),
            is_group: Some(false),
            is_im: Some(true),
            is_mpim: Some(false),
            is_private: Some(true),
            is_archived: Some(false),
            unread_count: None,
            extra: HashMap::new(),
        });
    }
}

fn cached_states(cache_dir: &Path) -> Vec<CachedSearchState> {
    store::load_active_search_state(cache_dir)
        .ok()
        .flatten()
        .map(|state| CachedSearchState {
            version: CACHE_VERSION,
            workspace_id: state.workspace_id,
            conversations: state.conversations,
            user_names: state.user_names,
            user_full_names: state.user_full_names,
            user_search_aliases: state.user_search_aliases,
        })
        .into_iter()
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

    fn write_index(directory: &Path, state: serde_json::Value) {
        write_index_version(directory, state, CACHE_VERSION);
    }

    fn write_index_version(directory: &Path, mut state: serde_json::Value, version: u32) {
        const KEY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        fs::create_dir_all(directory).unwrap();
        state["version"] = serde_json::json!(version);
        if version == CACHE_VERSION {
            let workspace_id = state["workspace_id"].as_str().unwrap();
            let conversations =
                serde_json::from_value::<Vec<SlackConversation>>(state["conversations"].clone())
                    .unwrap();
            let user_names = serde_json::from_value::<HashMap<String, String>>(
                state
                    .get("user_names")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            )
            .unwrap();
            let user_full_names = serde_json::from_value::<HashMap<String, String>>(
                state
                    .get("user_full_names")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            )
            .unwrap();
            let user_aliases = serde_json::from_value::<HashMap<String, Vec<String>>>(
                state
                    .get("user_search_aliases")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            )
            .unwrap();
            let store = store::WorkspaceStore::new(directory.to_path_buf(), workspace_id);
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    store.store_conversations(&conversations).await.unwrap();
                    store.store_user_names(&user_names).await.unwrap();
                    store.store_user_full_names(&user_full_names).await.unwrap();
                    store
                        .store_user_search_aliases(&user_aliases)
                        .await
                        .unwrap();
                    store.ensure_workspace_identity().await.unwrap();
                });
            return;
        }
        fs::write(directory.join("active-workspace"), KEY).unwrap();
        fs::write(directory.join(format!("{KEY}.json")), state.to_string()).unwrap();
    }

    fn write_legacy_cache(directory: &Path, key: &str, mut state: serde_json::Value) {
        fs::create_dir_all(directory).unwrap();
        state["version"] = serde_json::json!(CACHE_VERSION);
        fs::write(directory.join(format!("{key}.json")), state.to_string()).unwrap();
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
        write_index(
            &directory,
            serde_json::json!({
                "workspace_id": "T123:U0",
                "conversations": [
                    {"id": "C1", "name": "architecture", "is_channel": true},
                    {"id": "D1", "user": "U1", "is_im": true}
                ],
                "user_names": {"U1": "zilvinas"},
                "user_full_names": {"U1": "Žilvinas Kuusas"},
                "user_search_aliases": {"U1": ["zilvinas", "kuusas"]}
            }),
        );

        let dm = search(&directory, &["Zilvinas".into(), "Kuu".into()]);
        assert_eq!(dm.len(), 1);
        assert_eq!(dm[0].name, "Žilvinas Kuusas (zilvinas)");
        assert_eq!(dm[0].description, "Direct message");

        let channel = search(&directory, &["arch".into()]);
        assert_eq!(channel.len(), 1);
        assert_eq!(channel[0].name, "#architecture");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn searches_group_dms_without_the_current_user_and_skips_archived_results() {
        let directory = temp_dir();
        write_index(
            &directory,
            serde_json::json!({
                "workspace_id": "T123:U_SELF",
                "conversations": [
                    {"id": "M1", "is_mpim": true, "members": ["U_SELF", "U_FAT", "U_ROB"]},
                    {"id": "C_OLD", "name": "fat-rob-archive", "is_channel": true, "is_archived": true},
                    {"id": "UNKNOWN", "name": "fat rob unknown"}
                ],
                "user_names": {
                    "U_SELF": "Vincent",
                    "U_FAT": "Fatima",
                    "U_ROB": "Robey"
                }
            }),
        );

        let results = search(&directory, &["fat".into(), "rob".into()]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Fatima, Robey");
        assert_eq!(results[0].description, "Group direct message");
        assert!(!results[0].name.contains("Vincent"));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn subsearch_reranks_the_full_catalog_after_a_broad_query() {
        let directory = temp_dir();
        let mut conversations = (0..21)
            .map(|index| {
                serde_json::json!({
                    "id": format!("C{index}"),
                    "name": format!("alpha-{index:02}"),
                    "is_channel": true
                })
            })
            .collect::<Vec<_>>();
        conversations.push(serde_json::json!({
            "id": "C_ARCH",
            "name": "architecture",
            "is_channel": true
        }));
        write_index(
            &directory,
            serde_json::json!({
                "workspace_id": "T123:U0",
                "conversations": conversations
            }),
        );

        let first = search(&directory, &["a".into()]);
        assert_eq!(first.len(), 20);
        assert!(first.iter().all(|result| result.name != "#architecture"));

        let previous_ids = first
            .into_iter()
            .map(|result| result.id)
            .collect::<Vec<_>>();
        let refined = subsearch(&directory, &previous_ids, &["arc".into()]);
        assert_eq!(refined.len(), 1);
        assert_eq!(refined[0].name, "#architecture");

        let refined_from_empty = subsearch(&directory, &[], &["architecture".into()]);
        assert_eq!(refined_from_empty.len(), 1);
        assert_eq!(refined_from_empty[0].name, "#architecture");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn result_metadata_is_complete_and_omits_archived_targets() {
        let directory = temp_dir();
        write_index(
            &directory,
            serde_json::json!({
                "workspace_id": "T123:U0",
                "conversations": [
                    {"id": "D1", "user": "U1", "is_im": true},
                    {"id": "C_OLD", "name": "old", "is_channel": true, "is_archived": true}
                ],
                "user_names": {"U1": "Žilvinas Kuusas"}
            }),
        );
        let dm = encode_target(&ResultTarget {
            workspace_id: "T123:U0".into(),
            channel_id: "D1".into(),
        });
        let archived = encode_target(&ResultTarget {
            workspace_id: "T123:U0".into(),
            channel_id: "C_OLD".into(),
        });

        let metas = result_metas(&directory, &[dm.clone(), archived]);
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0]["id"].get::<String>().as_deref(), Some(dm.as_str()));
        assert_eq!(
            metas[0]["name"].get::<String>().as_deref(),
            Some("Žilvinas Kuusas")
        );
        assert_eq!(
            metas[0]["description"].get::<String>().as_deref(),
            Some("Direct message")
        );
        assert!(metas[0].contains_key("icon"));
        assert!(!metas[0].contains_key("gicon"));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn dbus_interface_and_shell_metadata_agree() {
        let interface = gio::DBusNodeInfo::for_xml(INTERFACE_XML)
            .unwrap()
            .lookup_interface("org.gnome.Shell.SearchProvider2")
            .unwrap();
        for method in [
            "GetInitialResultSet",
            "GetSubsearchResultSet",
            "GetResultMetas",
            "ActivateResult",
            "LaunchSearch",
        ] {
            assert!(
                interface.lookup_method(method).is_some(),
                "missing {method}"
            );
        }

        let metadata = include_str!("../data/eu.vanadrighem.conduit.search-provider.ini");
        assert!(metadata.contains("DesktopId=eu.vanadrighem.conduit.desktop"));
        assert!(metadata.contains("BusName=eu.vanadrighem.conduit"));
        assert!(metadata.contains(&format!("ObjectPath={OBJECT_PATH}")));
        assert!(metadata.contains("Version=2"));

        let desktop = include_str!("../data/eu.vanadrighem.conduit.desktop.in");
        assert!(desktop.contains("DBusActivatable=true"));
    }

    #[test]
    fn migrates_a_single_legacy_workspace_for_provider_only_startup() {
        const KEY: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let directory = temp_dir();
        write_legacy_cache(
            &directory,
            KEY,
            serde_json::json!({
                "workspace_id": "T123:U0",
                "conversations": [
                    {"id": "C1", "name": "general", "is_channel": true}
                ],
                "messages": {"C1": [{"text": "must not enter the search index"}]}
            }),
        );

        let results = search(&directory, &["gen".into()]);

        assert_eq!(results.len(), 1);
        assert!(directory.join("state.sqlite3").exists());
        let state = store::load_active_search_state(&directory)
            .unwrap()
            .unwrap();
        assert_eq!(state.workspace_id, "T123:U0");
        assert_eq!(state.conversations.len(), 1);
        assert!(!directory.join(format!("{KEY}.json")).exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn refuses_to_guess_between_multiple_legacy_workspaces() {
        const KEY_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const KEY_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let directory = temp_dir();
        for (key, workspace) in [(KEY_A, "T1:U1"), (KEY_B, "T2:U2")] {
            write_legacy_cache(
                &directory,
                key,
                serde_json::json!({
                    "workspace_id": workspace,
                    "conversations": [
                        {"id": "C1", "name": "general", "is_channel": true}
                    ]
                }),
            );
        }

        assert!(search(&directory, &["general".into()]).is_empty());
        assert!(!directory.join("active-workspace").exists());
        assert!(store::load_active_search_state(&directory)
            .unwrap()
            .is_none());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn ignores_invalid_legacy_cache_candidates() {
        const KEY: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
        let directory = temp_dir();
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join(format!("{KEY}.json")),
            serde_json::json!({
                "version": CACHE_VERSION + 1,
                "workspace_id": "T1:U1",
                "conversations": [{"id": "C1", "name": "general", "is_channel": true}]
            })
            .to_string(),
        )
        .unwrap();

        assert!(search(&directory, &["general".into()]).is_empty());
        assert!(!directory.join("active-workspace").exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn ignores_empty_queries_and_unidentified_legacy_caches() {
        let directory = temp_dir();
        write_index_version(
            &directory,
            serde_json::json!({
                "workspace_id": "T123:U0",
                "conversations": [{"id": "C1", "name": "general", "is_channel": true}]
            }),
            CACHE_VERSION + 1,
        );

        assert!(search(&directory, &[" ".into()]).is_empty());
        assert!(search(&directory, &["general".into()]).is_empty());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn searches_virtual_direct_messages_for_workspace_users_without_active_dm() {
        let directory = temp_dir();
        write_index(
            &directory,
            serde_json::json!({
                "workspace_id": "T123:U0",
                "conversations": [
                    {"id": "C1", "name": "architecture", "is_channel": true}
                ],
                "user_names": {"U_TESSI": "tessi"},
                "user_full_names": {"U_TESSI": "Tessier Smith"},
                "user_search_aliases": {"U_TESSI": ["tessi", "tessier"]}
            }),
        );

        let results = search(&directory, &["tessi".into()]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Tessier Smith (tessi)");
        assert_eq!(results[0].description, "Direct message");
        assert_eq!(decode_target(&results[0].id).unwrap().channel_id, "U_TESSI");

        let metas = result_metas(&directory, &[results[0].id.clone()]);
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0]["id"].get::<String>().unwrap(), results[0].id);
        assert_eq!(
            metas[0]["name"].get::<String>().unwrap(),
            "Tessier Smith (tessi)"
        );
        assert_eq!(
            metas[0]["description"].get::<String>().unwrap(),
            "Direct message"
        );

        let _ = fs::remove_dir_all(directory);
    }
}
