//! Bridges egui's immediate-mode loop and the async backend: a tokio runtime
//! runs the backend; the UI sends commands and drains events every frame.

use std::sync::{mpsc, Arc};

use chat_core::backend::ProviderFactory;
use chat_core::{Backend, BackendEvent, Command};
use connectors::ReqwestTransport;
use eframe::egui;
use tokio::sync::mpsc as tmpsc;

pub struct Runtime {
    _rt: tokio::runtime::Runtime,
    commands: tmpsc::UnboundedSender<Command>,
    events: mpsc::Receiver<BackendEvent>,
}

impl Runtime {
    pub fn start(ctx: egui::Context) -> std::io::Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let (cmd_tx, mut cmd_rx) = tmpsc::unbounded_channel::<Command>();
        let (ev_tx, ev_rx) = mpsc::channel::<BackendEvent>();

        let http = reqwest::Client::new();
        let factory: ProviderFactory = {
            let http = http.clone();
            Arc::new(move |cfg| llm::build_provider(cfg, http.clone()))
        };
        let sink = Arc::new(move |e: BackendEvent| {
            let _ = ev_tx.send(e);
            ctx.request_repaint();
        });
        let mut backend = Backend::new(sink, factory, Arc::new(ReqwestTransport::new(http)));
        rt.spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                backend.handle(cmd).await;
            }
        });
        Ok(Self {
            _rt: rt,
            commands: cmd_tx,
            events: ev_rx,
        })
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.commands.send(cmd);
    }

    pub fn drain(&self) -> impl Iterator<Item = BackendEvent> + '_ {
        self.events.try_iter()
    }
}
