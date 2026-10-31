//! The low-level version handshake and account login.

use eve_marshal::Value;
use eve_proto::{decode_payload, llv, MachoPacket, MessageType, VersionChallenge};
use tracing::{debug, error, info, warn};

use super::{Conn, ConnError, Result};
use crate::bulkdata;
use crate::db;
use crate::exceptions::user_error;
use crate::session::{ROLE_LOGIN, ROLE_PLAYER};

impl Conn {
    /// Exchange low-level version challenges; the client sends nothing else first.
    /// The server's carries how many are playing, which the login screen
    /// shows as its status (login.py GetServerStatus).
    pub(super) async fn handshake(&mut self) -> Result {
        let playing = self.server.world.lock().online_count();
        let challenge = VersionChallenge::server(playing);
        self.write_value(&challenge.to_value()).await?;

        let Some(frame) = self.read_frame().await? else {
            return Err(ConnError::Protocol("no low-level version reply".into()));
        };
        if frame.len() > llv::MAX_CLIENT_LLV_BYTES {
            return self
                .close("A low-level-version challenge response may not exceed 1000 bytes")
                .await;
        }
        let value = decode_payload(&frame, &self.server.table)?;
        let peer = match VersionChallenge::from_value(&value).and_then(|c| {
            c.validate()?;
            Ok(c)
        }) {
            Ok(c) => c,
            Err(e) => {
                return self
                    .close(format!(
                        "There was a low-level protocol version mismatch. \
                         My protocol version={0}, other protocol version={0}. ({e})",
                        llv::MACHO_VERSION
                    ))
                    .await;
            }
        };
        debug!(
            "handshake ok: build {} {:?}",
            peer.boot_build,
            String::from_utf8_lossy(&peer.codename)
        );
        Ok(())
    }

    pub(super) async fn authenticate(&mut self, packet: MachoPacket) -> Result {
        let clientinfo = packet.body.first().cloned().unwrap_or(Value::None);
        let username = string_of(packet.body.get(1));
        let password = string_of(packet.body.get(2));

        if let Err(reason) = check_version(&clientinfo) {
            warn!("client version rejected: {reason}");
            let err = user_error(
                "VersionMismatch",
                &[("srv", Value::bytes("Blood 2.1(3913)")), ("cli", Value::bytes(reason.clone()))],
            );
            self.send_error(&packet, err).await?;
            return self
                .close(format!("There was a low-level protocol version mismatch. {reason}"))
                .await;
        }

        let account = match db::authenticate(&self.server.db, &username, &password).await {
            Ok(a) => a,
            Err(e) => {
                error!("account lookup for {username:?} failed: {err}", err = crate::logging::chain(&e));
                self.send_error(&packet, user_error("AutClusterStarting", &[])).await?;
                return Err(e.into());
            }
        };
        let Some(account) = account else {
            info!("login rejected for {username:?}");
            return self.send_error(&packet, user_error("LoginAuthFailed", &[])).await;
        };

        self.session.user_id = Some(account.account_id);
        self.session.role = ROLE_LOGIN | ROLE_PLAYER | account.role;
        self.session.address = Some(self.peer.to_string());
        self.session.language_id = Some(
            clientinfo
                .get_str("languageID")
                .and_then(|v| v.as_bytes())
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_else(|| "EN".to_string()),
        );
        self.session.char_id = None;
        self.authenticated = true;
        self.span.record("user", account.account_id);

        self.pending_session_change = Some(self.session_change(self.session.change_body()));

        let inner = Value::Tuple(vec![self.server.bulk.init_vals(), bulkdata::service_info()]);
        let payload = eve_marshal::save(&inner, &self.server.table)?;
        let mut rsp = packet.respond(
            MessageType::AuthenticationRsp,
            vec![
                server_info(),
                Value::Int(account.account_id),
                Value::Long(self.session.role),
                Value::Bytes(payload),
                Value::Int(self.server.node_id),
            ],
        );
        rsp.user_id = self.session.user_id;
        self.send_packet(&rsp).await?;

        info!(
            role = self.session.role,
            sid = self.session.sid,
            "logged in as {:?}, sent {} init stubs",
            account.name,
            self.server.bulk.entries.len()
        );
        Ok(())
    }
}

fn server_info() -> Value {
    Value::Dict(vec![
        (Value::bytes("version.number"), Value::Double(llv::BOOT_VERSION)),
        (Value::bytes("version.build"), Value::Int(llv::BOOT_BUILD)),
        (Value::bytes("version.project"), Value::Bytes(llv::BOOT_CODENAME.to_vec())),
    ])
}

fn check_version(clientinfo: &Value) -> std::result::Result<(), String> {
    let number = clientinfo.get_str("version.number");
    let build = clientinfo.get_str("version.build").and_then(|v| v.as_i64());
    let project = clientinfo.get_str("version.project").and_then(|v| v.as_bytes());

    let number_ok = matches!(number, Some(Value::Double(d)) if *d == llv::BOOT_VERSION);
    if !number_ok {
        return Err(format!("version.number {number:?} != {}", llv::BOOT_VERSION));
    }
    if project != Some(llv::BOOT_CODENAME) {
        return Err(format!(
            "version.project {:?} != {:?}",
            project.map(String::from_utf8_lossy),
            String::from_utf8_lossy(llv::BOOT_CODENAME)
        ));
    }
    match build {
        Some(b) if b >= llv::BOOT_BUILD => Ok(()),
        other => Err(format!("version.build {other:?} < {}", llv::BOOT_BUILD)),
    }
}

fn string_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::Unicode(s)) => s.clone(),
        Some(Value::Bytes(b)) | Some(Value::Buffer(b)) => b.iter().map(|&c| c as char).collect(),
        _ => String::new(),
    }
}
