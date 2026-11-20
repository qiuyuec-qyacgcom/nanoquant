use crate::{auth, db, events, kline_stream, runtime};

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) started_at: i64,
    pub(crate) auth: auth::AuthState,
    pub(crate) db: Option<db::Db>,
    pub(crate) engine_control: runtime::EngineControl,
    pub(crate) events: events::EventBus,
    pub(crate) klines: kline_stream::KlineHub,
}
