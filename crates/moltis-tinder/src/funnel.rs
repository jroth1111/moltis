use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunnelState {
    Matched,
    OpenerSent,
    Engaged,
    DateProposed,
    NumberSecured,
    Closed,
}

impl FunnelState {
    pub fn can_advance_to(&self, next: &FunnelState) -> bool {
        matches!(
            (self, next),
            (FunnelState::Matched, FunnelState::OpenerSent)
                | (FunnelState::OpenerSent, FunnelState::Engaged)
                | (FunnelState::Engaged, FunnelState::DateProposed)
                | (FunnelState::Engaged, FunnelState::NumberSecured)
                | (FunnelState::DateProposed, FunnelState::NumberSecured)
                | (FunnelState::Matched, FunnelState::Closed)
                | (FunnelState::OpenerSent, FunnelState::Closed)
                | (FunnelState::Engaged, FunnelState::Closed)
                | (FunnelState::DateProposed, FunnelState::Closed)
                | (FunnelState::NumberSecured, FunnelState::Closed)
        )
    }
}

impl fmt::Display for FunnelState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            FunnelState::Matched => "matched",
            FunnelState::OpenerSent => "opener_sent",
            FunnelState::Engaged => "engaged",
            FunnelState::DateProposed => "date_proposed",
            FunnelState::NumberSecured => "number_secured",
            FunnelState::Closed => "closed",
        };
        f.write_str(s)
    }
}

impl FromStr for FunnelState {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "matched" => Ok(FunnelState::Matched),
            "opener_sent" => Ok(FunnelState::OpenerSent),
            "engaged" => Ok(FunnelState::Engaged),
            "date_proposed" => Ok(FunnelState::DateProposed),
            "number_secured" => Ok(FunnelState::NumberSecured),
            "closed" => Ok(FunnelState::Closed),
            other => Err(anyhow::anyhow!("unknown funnel state: {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TinderMatch {
    pub id: String,
    pub name: String,
    pub funnel_state: FunnelState,
    pub exchange_count: i64,
    pub last_message_ts: Option<i64>,
    pub notes: String,
    pub created_at: i64,
    pub updated_at: i64,
}

use crate::util::now_ms;

pub async fn get_match(pool: &sqlx::SqlitePool, id: &str) -> anyhow::Result<Option<TinderMatch>> {
    let row = sqlx::query_as::<_, MatchRow>(
        "SELECT id, name, funnel_state, exchange_count, last_message_ts, notes, created_at, updated_at \
         FROM tinder_matches WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => Ok(Some(r.into_match()?)),
        None => Ok(None),
    }
}

pub async fn list_matches(
    pool: &sqlx::SqlitePool,
    state_filter: Option<&FunnelState>,
) -> anyhow::Result<Vec<TinderMatch>> {
    let rows = match state_filter {
        Some(state) => {
            sqlx::query_as::<_, MatchRow>(
                "SELECT id, name, funnel_state, exchange_count, last_message_ts, notes, created_at, updated_at \
                 FROM tinder_matches WHERE funnel_state = ? ORDER BY updated_at DESC",
            )
            .bind(state.to_string())
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as::<_, MatchRow>(
                "SELECT id, name, funnel_state, exchange_count, last_message_ts, notes, created_at, updated_at \
                 FROM tinder_matches ORDER BY updated_at DESC",
            )
            .fetch_all(pool)
            .await?
        }
    };
    rows.into_iter().map(|r| r.into_match()).collect()
}

pub async fn upsert_match(pool: &sqlx::SqlitePool, m: &TinderMatch) -> anyhow::Result<()> {
    let state_str = m.funnel_state.to_string();
    sqlx::query(
        "INSERT INTO tinder_matches (id, name, funnel_state, exchange_count, last_message_ts, notes, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET \
           name = excluded.name, \
           funnel_state = excluded.funnel_state, \
           exchange_count = excluded.exchange_count, \
           last_message_ts = excluded.last_message_ts, \
           notes = excluded.notes, \
           updated_at = excluded.updated_at",
    )
    .bind(&m.id)
    .bind(&m.name)
    .bind(&state_str)
    .bind(m.exchange_count)
    .bind(m.last_message_ts)
    .bind(&m.notes)
    .bind(m.created_at)
    .bind(m.updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_funnel(
    pool: &sqlx::SqlitePool,
    id: &str,
    new_state: FunnelState,
) -> anyhow::Result<()> {
    let now = now_ms();
    let result = sqlx::query(
        "UPDATE tinder_matches SET funnel_state = ?, updated_at = ? WHERE id = ?",
    )
    .bind(new_state.to_string())
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        anyhow::bail!("match not found: {id}");
    }
    Ok(())
}

pub async fn increment_exchange(pool: &sqlx::SqlitePool, id: &str) -> anyhow::Result<()> {
    let now = now_ms();
    let result = sqlx::query(
        "UPDATE tinder_matches SET exchange_count = exchange_count + 1, last_message_ts = ?, updated_at = ? WHERE id = ?",
    )
    .bind(now)
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        anyhow::bail!("match not found: {id}");
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct MatchRow {
    id: String,
    name: String,
    funnel_state: String,
    exchange_count: i64,
    last_message_ts: Option<i64>,
    notes: String,
    created_at: i64,
    updated_at: i64,
}

impl MatchRow {
    fn into_match(self) -> anyhow::Result<TinderMatch> {
        let funnel_state: FunnelState = self.funnel_state.parse()?;
        Ok(TinderMatch {
            id: self.id,
            name: self.name,
            funnel_state,
            exchange_count: self.exchange_count,
            last_message_ts: self.last_message_ts,
            notes: self.notes,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}
