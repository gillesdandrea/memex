mod client;
mod platform;
#[path = "../resources.rs"]
mod resources;
mod store;

use qtbridge::{QApp, qobject};
use serde_json::{Value, json};
use std::sync::mpsc::{self, Receiver, Sender};

pub struct Backend {
    sender: Sender<Value>,
    receiver: Receiver<Value>,
    store: store::Store,
}

impl Default for Backend {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            sender,
            receiver,
            store: store::Store::default(),
        }
    }
}

#[qobject]
impl Backend {
    #[qslot]
    fn action(&mut self, payload: String) -> String {
        if let Ok(value) = serde_json::from_str(&payload) {
            self.store.action(value);
        }
        self.state()
    }

    #[qslot]
    fn state(&mut self) -> String {
        self.store
            .poll_changed()
            .map_or_else(String::new, |value| value.to_string())
    }

    #[qslot]
    fn request(&self, id: String, payload: String) {
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            let result = serde_json::from_str(&payload)
                .map_err(|error| error.to_string())
                .and_then(dispatch);
            let message = match result {
                Ok(value) => json!({"id": id, "result": value}),
                Err(error) => json!({"id": id, "error": error}),
            };
            let _ = sender.send(message);
        });
    }

    #[qslot]
    fn poll(&self) -> String {
        serde_json::to_string(&self.receiver.try_iter().collect::<Vec<_>>()).unwrap()
    }
}

fn dispatch(value: Value) -> Result<Value, String> {
    let session = value.get("session").cloned().unwrap_or(Value::Null);
    match value["op"].as_str().unwrap_or("") {
        "destinations" => Ok(platform::destinations()),
        "resume" => platform::resume(session, value["destination"].as_str().unwrap_or("").into())
            .map(|()| Value::Null),
        "reveal" => platform::reveal(session).map(|()| Value::Null),
        "source_preview" => {
            platform::source_preview(session, value["url"].as_str().unwrap_or("").into())
        }
        _ => client::request(value),
    }
}

pub fn run() -> i32 {
    register_qml_resources();
    QApp::new()
        .application_name("Memex")
        .register::<Backend>()
        .load_qml_from_file("qrc:/qml/Main.qml")
        .run()
}

/// Register the same embedded UI for the application and native test harness.
pub fn register_qml_resources() {
    resources::register();
}
