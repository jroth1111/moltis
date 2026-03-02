use moltis_cron::types::{
    CronJobCreate, CronPayload, CronSchedule, CronSandboxConfig, SessionTarget,
};

/// Daily session: run tinder workflow every evening at 7pm.
pub fn daily_session() -> CronJobCreate {
    CronJobCreate {
        id: Some("tinder-daily-session".to_string()),
        name: "tinder-daily-session".to_string(),
        schedule: CronSchedule::Cron {
            expr: "0 19 * * *".to_string(),
            tz: None,
        },
        payload: CronPayload::AgentTurn {
            message: "Run the full tinder workflow: swipe, send openers, reply to engaged matches."
                .to_string(),
            model: None,
            timeout_secs: Some(1800),
            deliver: false,
            channel: None,
            to: None,
        },
        session_target: SessionTarget::Named("tinder-main".to_string()),
        delete_after_run: false,
        enabled: true,
        system: false,
        sandbox: CronSandboxConfig::default(),
        wake_mode: Default::default(),
    }
}

/// Hourly replies: check for new messages from engaged matches.
pub fn hourly_replies() -> CronJobCreate {
    CronJobCreate {
        id: Some("tinder-hourly-replies".to_string()),
        name: "tinder-hourly-replies".to_string(),
        schedule: CronSchedule::Cron {
            expr: "0 * * * *".to_string(),
            tz: None,
        },
        payload: CronPayload::AgentTurn {
            message: "Check tinder matches in state=engaged for new replies. \
                      Process each reply with appropriate response. \
                      Call tinder_funnel action=list first."
                .to_string(),
            model: None,
            timeout_secs: Some(900),
            deliver: false,
            channel: None,
            to: None,
        },
        session_target: SessionTarget::Named("tinder-replies".to_string()),
        delete_after_run: false,
        enabled: true,
        system: false,
        sandbox: CronSandboxConfig::default(),
        wake_mode: Default::default(),
    }
}

/// Ghost recovery: re-engage matches that went silent after opener.
/// Runs Monday at 9am.
pub fn ghost_recovery() -> CronJobCreate {
    CronJobCreate {
        id: Some("tinder-ghost-recovery".to_string()),
        name: "tinder-ghost-recovery".to_string(),
        schedule: CronSchedule::Cron {
            expr: "0 9 * * 1".to_string(),
            tz: None,
        },
        payload: CronPayload::AgentTurn {
            message: "Find tinder matches where funnel_state=opener_sent AND \
                      last_message_ts < (now - 7 days). For each ghost, attempt \
                      a recovery message via tinder_browser."
                .to_string(),
            model: None,
            timeout_secs: Some(1800),
            deliver: false,
            channel: None,
            to: None,
        },
        session_target: SessionTarget::Named("tinder-recovery".to_string()),
        delete_after_run: false,
        enabled: true,
        system: false,
        sandbox: CronSandboxConfig::default(),
        wake_mode: Default::default(),
    }
}

/// System liveness check every 5 minutes.
pub fn system_liveness() -> CronJobCreate {
    CronJobCreate {
        id: Some("tinder-system-liveness".to_string()),
        name: "tinder-system-liveness".to_string(),
        schedule: CronSchedule::Cron {
            expr: "*/5 * * * *".to_string(),
            tz: None,
        },
        payload: CronPayload::SystemEvent {
            text: "tinder-liveness-check".to_string(),
        },
        session_target: SessionTarget::Named("system-liveness".to_string()),
        delete_after_run: false,
        enabled: true,
        system: true,
        sandbox: CronSandboxConfig::default(),
        wake_mode: Default::default(),
    }
}
