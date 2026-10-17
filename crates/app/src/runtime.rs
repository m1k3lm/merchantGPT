//! Bridges egui's immediate-mode loop and the async backend: a tokio runtime
//! runs the backend; the UI sends commands and drains events every frame.

use std::sync::{mpsc, Arc};

use chat_core::backend::ProviderFactory;
use chat_core::{Backend, BackendEvent, Command, Config};
use connectors::ReqwestTransport;
use eframe::egui;
use store::{KeyringKeyStore, Store};
use tokio::sync::mpsc as tmpsc;

pub struct Runtime {
    _rt: tokio::runtime::Runtime,
    commands: tmpsc::UnboundedSender<Command>,
    events: mpsc::Receiver<BackendEvent>,
    pub config: Config,
}

impl Runtime {
    /// Loads the config, opens the encrypted store and starts the backend.
    /// Errors are fatal and shown to the merchant instead of the chat.
    pub fn start(ctx: egui::Context) -> Result<Self, String> {
        let config = match Config::default_path() {
            Some(path) => {
                let _ = Config::write_template_if_missing(&path);
                Config::load(&path)?
            }
            None => Config::default(),
        };
        let db_path = config.db_path();
        let store = Store::open_with_keystore(&db_path, &KeyringKeyStore::default())
            .map_err(|e| format!("Could not open {}: {e}", db_path.display()))?;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
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
        let mut backend = Backend::new(sink, factory, Arc::new(ReqwestTransport::new(http)), store)
            .with_agent_defaults(config.agent_config());
        rt.spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                backend.handle(cmd).await;
            }
        });
        let _ = cmd_tx.send(Command::Init);
        Ok(Self {
            _rt: rt,
            commands: cmd_tx,
            events: ev_rx,
            config,
        })
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.commands.send(cmd);
    }

    pub fn drain(&self) -> impl Iterator<Item = BackendEvent> + '_ {
        self.events.try_iter()
    }
}
