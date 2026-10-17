//! The root of the supervision tree.
//!
//! One-for-one supervision, in the Erlang sense: each service is a linked
//! child actor, and when one dies the root restarts *that one* and leaves the
//! rest alone. The root itself never stops because of a child — `on_link_died`
//! always returns `Continue`.
//!
//! Two layers of recovery, which are easy to confuse:
//!
//! * the **process** died — handled inside [`crate::service::ServiceActor`]
//!   with a restart policy and backoff;
//! * the **actor** supervising that process died (a panic it could not absorb,
//!   or an explicit kill) — handled here by rebuilding the actor.
//!
//! The root knows every configured service, including `enabled = false` ones:
//! those have no actor until someone starts them through the control socket,
//! but they still appear in `status`.

use crate::config::Config;
use crate::logs::LogSink;
use crate::service::{new_status, ServiceActor, ServiceArgs, Stop};
use ::supervisor::control::ServiceStatus;
use kameo::actor::{Actor, ActorID, ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible, PanicError};
use kameo::message::{Context, Message};
use kameo::Reply;
use metrics::counter;
use std::ops::ControlFlow;

/// Ask the tree to stop every service and then itself.
#[derive(Debug, Clone, Copy)]
pub struct Shutdown;

/// How many services currently have a supervising actor.
#[derive(Debug, Clone, Copy)]
pub struct ServiceCount;

/// Status of every configured service, in declaration order.
#[derive(Debug, Clone, Copy)]
pub struct StatusAll;

/// Status of one service, `None` if there is no such service.
#[derive(Debug, Clone)]
pub struct StatusOf(pub String);

/// Looks a service up for the control socket.
///
/// The root only hands out the actor; the control connection then talks to
/// it directly, so a `stop` that waits out a slow child never blocks the root
/// (and with it the rebuilding of other services' actors).
#[derive(Debug, Clone)]
pub struct Resolve {
    pub name: String,
    /// Spawn an actor if the service has none (a disabled service being
    /// started at runtime). The new actor starts the process itself.
    pub spawn: bool,
}

/// Answer to [`Resolve`].
#[derive(Debug, Reply)]
pub enum Resolved {
    Unknown,
    ShuttingDown,
    /// The service exists but has no actor, and `spawn` was false.
    Idle {
        critical: bool,
    },
    Actor {
        actor: ActorRef<ServiceActor>,
        critical: bool,
        /// True if this request created the actor, which already started
        /// the process.
        spawned: bool,
    },
}

/// One configured service and, if it is being supervised, its actor.
struct Entry {
    args: ServiceArgs,
    actor: Option<ActorRef<ServiceActor>>,
}

pub struct Supervisor {
    /// Every configured service, in declaration order.
    entries: Vec<Entry>,
    logs: LogSink,
    stopping: bool,
}

impl Supervisor {
    /// Builds the per-service arguments for every configured service, enabled
    /// or not; only enabled ones are started by [`Actor::on_start`].
    pub fn plan(config: &Config, logs: LogSink) -> Vec<ServiceArgs> {
        config
            .services
            .iter()
            .map(|service| ServiceArgs {
                program: config.resolve(&service.command),
                working_dir: service
                    .working_dir
                    .clone()
                    .map(|dir| config.resolve_data(&dir))
                    .unwrap_or_else(|| config.data_dir.clone()),
                service: service.clone(),
                logs: logs.clone(),
                status: new_status(service.enabled),
            })
            .collect()
    }

    async fn start_child(&mut self, actor_ref: &ActorRef<Self>, index: usize) {
        let args = self.entries[index].args.clone();
        let child = ServiceActor::spawn_link(actor_ref, args).await;
        self.entries[index].actor = Some(child);
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.args.service.name == name)
    }
}

impl Actor for Supervisor {
    type Args = (Vec<ServiceArgs>, LogSink);
    type Error = Infallible;

    fn name() -> &'static str {
        "supervisor"
    }

    async fn on_start(
        (plan, logs): Self::Args,
        actor_ref: ActorRef<Self>,
    ) -> Result<Self, Self::Error> {
        let mut supervisor = Supervisor {
            entries: plan
                .into_iter()
                .map(|args| Entry { args, actor: None })
                .collect(),
            logs,
            stopping: false,
        };
        // Declaration order is start order. The watchdog feeder goes first so
        // the timer is being fed before anything slower comes up.
        for index in 0..supervisor.entries.len() {
            let service = &supervisor.entries[index].args.service;
            if !service.enabled {
                tracing::info!(service = %service.name, "disabled, not starting");
                continue;
            }
            tracing::info!(service = %service.name, "supervising");
            supervisor.start_child(&actor_ref, index).await;
        }
        Ok(supervisor)
    }

    /// A child actor died. Rebuild it; never let it stop the root.
    async fn on_link_died(
        &mut self,
        actor_ref: WeakActorRef<Self>,
        id: ActorID,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        if self.stopping {
            return Ok(ControlFlow::Continue(()));
        }
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.actor.as_ref().is_some_and(|actor| actor.id() == id))
        else {
            return Ok(ControlFlow::Continue(()));
        };
        self.entries[index].actor = None;
        let name = self.entries[index].args.service.name.clone();

        if matches!(reason, ActorStopReason::Normal) {
            tracing::info!(service = %name, "service actor stopped normally");
            return Ok(ControlFlow::Continue(()));
        }

        tracing::error!(service = %name, ?reason, "service actor died, rebuilding");
        self.logs.note(
            &name,
            format!("supervisor actor died ({reason:?}), rebuilding"),
        );
        self.logs.alert(
            &name,
            format!("supervisor actor died ({reason:?}), rebuilding it"),
            self.entries[index].args.service.critical,
        );
        counter!("kamoder_supervisor_actor_restarts_total", "service" => name).increment(1);

        if let Some(actor_ref) = actor_ref.upgrade() {
            self.start_child(&actor_ref, index).await;
        }
        Ok(ControlFlow::Continue(()))
    }

    /// The root absorbing its own panic is what keeps the tree alive.
    async fn on_panic(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        err: PanicError,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        tracing::error!(panic = %err, "supervisor panicked, continuing");
        self.logs
            .alert("supervisor", format!("panicked, continuing: {err}"), true);
        counter!("kamoder_supervisor_root_panics_total").increment(1);
        Ok(ControlFlow::Continue(()))
    }
}

impl Message<Shutdown> for Supervisor {
    type Reply = ();

    async fn handle(&mut self, _msg: Shutdown, _ctx: &mut Context<Self, Self::Reply>) {
        self.stopping = true;
        // Stop in reverse start order, so the watchdog feeder is the last
        // thing still running while everything else winds down.
        for entry in self.entries.iter_mut().rev() {
            let Some(child) = entry.actor.take() else {
                continue;
            };
            tracing::info!(service = %entry.args.service.name, "stopping");
            // `Stop` terminates the process; `stop_gracefully` then drains the
            // actor's mailbox and runs `on_stop`.
            let _ = child.tell(Stop).await;
            let _ = child.stop_gracefully().await;
            child.wait_for_shutdown().await;
        }
        self.logs.flush();
    }
}

impl Message<ServiceCount> for Supervisor {
    type Reply = usize;

    async fn handle(&mut self, _msg: ServiceCount, _ctx: &mut Context<Self, Self::Reply>) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.actor.is_some())
            .count()
    }
}

impl Message<StatusAll> for Supervisor {
    type Reply = Vec<ServiceStatus>;

    async fn handle(
        &mut self,
        _msg: StatusAll,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Vec<ServiceStatus> {
        self.entries
            .iter()
            .map(|entry| entry.args.report())
            .collect()
    }
}

impl Message<StatusOf> for Supervisor {
    type Reply = Option<ServiceStatus>;

    async fn handle(
        &mut self,
        msg: StatusOf,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Option<ServiceStatus> {
        self.position(&msg.0)
            .map(|index| self.entries[index].args.report())
    }
}

impl Message<Resolve> for Supervisor {
    type Reply = Resolved;

    async fn handle(&mut self, msg: Resolve, ctx: &mut Context<Self, Self::Reply>) -> Resolved {
        if self.stopping {
            return Resolved::ShuttingDown;
        }
        let Some(index) = self.position(&msg.name) else {
            return Resolved::Unknown;
        };
        let critical = self.entries[index].args.service.critical;
        if let Some(actor) = &self.entries[index].actor {
            return Resolved::Actor {
                actor: actor.clone(),
                critical,
                spawned: false,
            };
        }
        if !msg.spawn {
            return Resolved::Idle { critical };
        }
        tracing::info!(service = %msg.name, "starting on request");
        self.start_child(&ctx.actor_ref(), index).await;
        match &self.entries[index].actor {
            Some(actor) => Resolved::Actor {
                actor: actor.clone(),
                critical,
                spawned: true,
            },
            None => Resolved::Idle { critical },
        }
    }
}
