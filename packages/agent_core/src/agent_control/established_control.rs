use std::net::SocketAddr;

use message_encoding::MessageEncoding;
use playit_agent_proto::control_feed::ControlFeed;
use playit_agent_proto::control_messages::{AgentRegistered, ControlRequest, ControlResponse, Ping, Pong};
use playit_agent_proto::rpc::ControlRpcMessage;

use crate::utils::now_milli;

use super::connected_control::ConnectedControl;
use super::errors::{ControlError, SetupError};
use super::{AuthResource, PacketIO};

pub struct EstablishedControl<A: AuthResource, IO: PacketIO> {
    pub(super) auth: A,
    pub(super) conn: ConnectedControl<IO>,
    pub(super) auth_pong: Pong,
    pub(super) registered: AgentRegistered,
    pub(super) current_ping: Option<u32>,
    pub(super) clock_offset: i64,
    pub(super) force_expired: bool,
}

impl<A: AuthResource, IO: PacketIO> EstablishedControl<A, IO> {
    pub async fn send_keep_alive(&mut self, request_id: u64) -> Result<(), ControlError> {
        self.send(ControlRpcMessage {
            request_id,
            content: ControlRequest::AgentKeepAlive(self.registered.id.clone()),
        }).await
    }

    pub async fn send_setup_udp_channel(&mut self, request_id: u64) -> Result<(), ControlError> {
        self.send(ControlRpcMessage {
            request_id,
            content: ControlRequest::SetupUdpChannel(self.registered.id.clone()),
        }).await
    }

    pub async fn send_ping(&mut self, request_id: u64, now: u64) -> Result<(), ControlError> {
        self.send(ControlRpcMessage {
            request_id,
            content: ControlRequest::Ping(Ping { now, current_ping: self.current_ping, session_id: Some(self.registered.id.clone()) }),
        }).await
    }

    pub fn get_expire_at(&self) -> u64 {
        self.registered.expires_at
    }

    pub fn is_expired(&self) -> bool {
        self.force_expired || self.auth_pong.session_expire_at.is_none() || self.flow_changed()
    }

    pub fn set_expired(&mut self) {
        self.force_expired = true;
    }

    fn flow_changed(&self) -> bool {
        self.conn.pong.client_addr != self.auth_pong.client_addr
    }

    async fn send(&mut self, req: ControlRpcMessage<ControlRequest>) -> Result<(), ControlError> {
        self.conn.send(&req).await?;
        Ok(())
    }

    pub async fn authenticate(&mut self) -> Result<(), SetupError> {
        let registered = self.conn.authenticate(&self.auth).await?;

        self.registered = registered;
        self.auth_pong = self.conn.pong.clone();

        tracing::info!(
            last_pong = ?self.auth_pong,
            "authenticate control"
        );

        Ok(())
    }

    pub fn into_connected(self) -> ConnectedControl<IO> {
        self.conn
    }

    pub async fn recv_feed_msg(&mut self) -> Result<ControlFeed, ControlError> {
        let feed = self.conn.recv().await?;
        
        if let ControlFeed::Response(res) = &feed {
            match &res.content {
                ControlResponse::AgentRegistered(registered) => {
                    tracing::info!(details = ?registered, "agent registered");
                    self.registered = registered.clone();
                }
                ControlResponse::Pong(pong) => {
                    let now = now_milli();
                    let rtt = (now.max(pong.request_now) - pong.request_now) as u32;

                    let server_ts = pong.server_now - (rtt / 2) as u64;
                    let local_ts = pong.request_now;
                    self.clock_offset = local_ts as i64 - server_ts as i64;

                    if 10_000 < self.clock_offset.abs() {
                        tracing::warn!("local timestamp if over 10 seconds off");
                    }

                    self.current_ping = Some(rtt);
                    self.auth_pong = pong.clone();

                    if let Some(expires_at) = pong.session_expire_at {
                        self.registered.expires_at = self.server_ts_to_local(expires_at);
                    }
                }
                _ => {}
            }
        }

        Ok(feed)
    }

    pub fn server_ts_to_local(&self, ts: u64) -> u64 {
        (ts as i64 + self.clock_offset) as u64
    }
}
