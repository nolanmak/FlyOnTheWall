//! Calendar settings: Apple Calendar status, and an optional Google Calendar
//! connection through Composio.
//!
//! The Composio key is the one secret this API accepts from the page. It is
//! written straight to the keychain by the daemon and never returned: the
//! response only says whether one is saved (KEY-01).

use serde::{Deserialize, Serialize};

/// Longest key the route accepts. Composio keys are well under this.
pub const MAX_KEY_LEN: usize = 512;

/// One Google account connected through Composio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleAccount {
    /// Composio's connected-account id.
    pub id: String,
    /// Where the connection stands.
    pub status: AccountState,
    /// When the user started connecting it, epoch milliseconds.
    pub added_at_ms: i64,
    /// The Google address, read from the account's primary calendar once it
    /// is active. Absent until then.
    #[serde(default)]
    pub email: Option<String>,
}

/// A connected account's state, as the page shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    /// Signed in; its calendars are read when a recording is saved.
    Active,
    /// Waiting for the user to finish Google sign-in.
    Pending,
    /// Sign-in failed, expired or was revoked. Disconnect and connect again.
    Failed,
}

/// What the settings section shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CalendarSettingsResponse {
    /// Apple Calendar access, in words ("granted", "not asked yet", ...).
    pub apple_access: String,
    /// Whether a Composio key is in the keychain. Never the key.
    pub composio_key_saved: bool,
    /// Whether the user accepted that event titles and times pass through
    /// Composio (KEY-04).
    pub composio_acknowledged: bool,
    /// Google accounts connected through Composio.
    pub google_accounts: Vec<GoogleAccount>,
    /// Whether the invite's names are given to the summary engine as
    /// spellings. Names only, never addresses.
    #[serde(default)]
    pub invitee_names: bool,
    /// A result to show, such as how many meetings had names fixed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    /// After `connect`: the Google sign-in page to open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_url: Option<String>,
    /// Why the last action failed.
    pub error: Option<String>,
}

/// What the page can ask for.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum CalendarAction {
    /// Check a Composio key and save it to the keychain.
    SaveKey {
        /// The pasted key.
        key: String,
    },
    /// Remove the Composio key from the keychain.
    ForgetKey,
    /// Start connecting another Google account.
    Connect {
        /// The user ticked the disclosure.
        #[serde(default)]
        acknowledge_egress: bool,
    },
    /// Re-check accounts still waiting on sign-in.
    Refresh,
    /// Disconnect one account, here and at Composio.
    Disconnect {
        /// Composio's connected-account id.
        id: String,
    },
    /// Turn sending invite names to the summary engine on or off.
    SetInviteeNames {
        /// On or off.
        enabled: bool,
    },
    /// Look up the invites of recent calendar-named meetings and fix
    /// misheard names in them.
    FixRecentNames,
}

impl std::fmt::Debug for CalendarAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SaveKey { .. } => f.write_str("SaveKey(<redacted>)"),
            Self::ForgetKey => f.write_str("ForgetKey"),
            Self::Connect { acknowledge_egress } => {
                write!(f, "Connect {{ acknowledge_egress: {acknowledge_egress} }}")
            }
            Self::Refresh => f.write_str("Refresh"),
            Self::Disconnect { id } => write!(f, "Disconnect({id})"),
            Self::SetInviteeNames { enabled } => write!(f, "SetInviteeNames({enabled})"),
            Self::FixRecentNames => f.write_str("FixRecentNames"),
        }
    }
}

impl CalendarAction {
    /// Refuse input no Composio call should ever see.
    ///
    /// # Errors
    /// A blank or oversized key, or an account id that is not a plain token.
    pub fn validated(self) -> Result<Self, String> {
        match &self {
            Self::SaveKey { key } => {
                let trimmed = key.trim();
                if trimmed.is_empty() {
                    return Err("Paste your Composio API key first.".into());
                }
                if trimmed.len() > MAX_KEY_LEN || trimmed.chars().any(char::is_whitespace) {
                    return Err("That doesn't look like a Composio API key.".into());
                }
            }
            Self::Disconnect { id } => {
                let plain = !id.is_empty()
                    && id.len() <= 128
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
                if !plain {
                    return Err("Unknown account.".into());
                }
            }
            Self::ForgetKey
            | Self::Connect { .. }
            | Self::Refresh
            | Self::SetInviteeNames { .. }
            | Self::FixRecentNames => {}
        }
        Ok(self)
    }
}

/// The daemon side: keychain, Composio calls and persistence.
pub trait CalendarControl: std::fmt::Debug + Send + Sync {
    /// Current status. Makes no network call.
    fn read(&self) -> CalendarSettingsResponse;
    /// Carry out `action`; `Ok(Some(url))` is a sign-in page to open.
    ///
    /// # Errors
    /// A message for the user.
    fn act(&self, action: CalendarAction) -> Result<Option<String>, String>;

    /// Fix misheard names in recent calendar-named meetings from their
    /// invites; returns how many meetings changed. May take a while: one
    /// calendar lookup per meeting.
    ///
    /// # Errors
    /// A message for the user.
    fn fix_recent_names(&self) -> Result<usize, String> {
        Err("Not available.".into())
    }
}

/// Authenticated GET.
pub async fn get(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
) -> axum::response::Response {
    let Some(control) = state.calendar() else {
        return crate::ingress::not_found();
    };
    match tokio::task::spawn_blocking(move || control.read()).await {
        Ok(doc) => crate::api::json(&state, &doc),
        Err(_) => crate::api::server_error(),
    }
}

/// Authenticated POST. Network work runs off the async runtime.
pub async fn post(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    body: Result<axum::Json<CalendarAction>, axum::extract::rejection::JsonRejection>,
) -> axum::response::Response {
    let Some(control) = state.calendar() else {
        return crate::ingress::not_found();
    };
    let Ok(axum::Json(action)) = body else {
        return crate::ingress::not_found();
    };
    match tokio::task::spawn_blocking(move || {
        let mut notice = None;
        let outcome = action.validated().and_then(|valid| {
            if valid == CalendarAction::FixRecentNames {
                let fixed = control.fix_recent_names()?;
                notice = Some(match fixed {
                    0 => "No misheard names found in recent meetings.".to_owned(),
                    1 => "Fixed names in 1 meeting.".to_owned(),
                    n => format!("Fixed names in {n} meetings."),
                });
                Ok(None)
            } else {
                control.act(valid)
            }
        });
        let mut response = control.read();
        response.notice = notice;
        match outcome {
            Ok(url) => response.redirect_url = url,
            Err(error) => response.error = Some(error),
        }
        response
    })
    .await
    {
        Ok(doc) => crate::api::json(&state, &doc),
        Err(_) => crate::api::server_error(),
    }
}
