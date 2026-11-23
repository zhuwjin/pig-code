//! Aggregated token-usage reporting over per-call `api_usage` rows.

use std::collections::BTreeSet;

use rusqlite::{Connection, params};

/// The four headline totals shown by the usage page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageSummary {
    total: u64,
    last_30_days: u64,
    last_7_days: u64,
    today: u64,
}

impl UsageSummary {
    pub fn new(total: u64, last_30_days: u64, last_7_days: u64, today: u64) -> Self {
        Self {
            total,
            last_30_days,
            last_7_days,
            today,
        }
    }

    pub fn total(self) -> u64 {
        self.total
    }

    pub fn last_30_days(self) -> u64 {
        self.last_30_days
    }

    pub fn last_7_days(self) -> u64 {
        self.last_7_days
    }

    pub fn today(self) -> u64 {
        self.today
    }
}

/// Tokens attributed to one local calendar day.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DailyUsage {
    day: String,
    tokens: u64,
}

impl DailyUsage {
    pub fn new(day: impl Into<String>, tokens: u64) -> Self {
        Self {
            day: day.into(),
            tokens,
        }
    }

    pub fn day(&self) -> &str {
        &self.day
    }

    pub fn tokens(&self) -> u64 {
        self.tokens
    }
}

/// One filtered usage report plus the unfiltered filter choices.
#[derive(Clone, Debug, Default)]
pub struct UsageStats {
    summary: UsageSummary,
    providers: Vec<String>,
    models: Vec<String>,
    /// Workspace paths (session cwd values) with recorded usage.
    workspaces: Vec<String>,
    daily: Vec<DailyUsage>,
}

impl UsageStats {
    pub fn new(
        summary: UsageSummary,
        providers: Vec<String>,
        models: Vec<String>,
        workspaces: Vec<String>,
        daily: Vec<DailyUsage>,
    ) -> Self {
        Self {
            summary,
            providers,
            models,
            workspaces,
            daily,
        }
    }

    pub fn summary(&self) -> UsageSummary {
        self.summary
    }

    pub fn providers(&self) -> &[String] {
        &self.providers
    }

    pub fn models(&self) -> &[String] {
        &self.models
    }

    pub fn workspaces(&self) -> &[String] {
        &self.workspaces
    }

    pub fn daily(&self) -> &[DailyUsage] {
        &self.daily
    }
}

/// Filters and local-calendar boundaries for one report.
#[derive(Clone, Debug)]
pub struct UsageQuery {
    provider: Option<String>,
    model: Option<String>,
    /// Workspace path (session cwd) to restrict the report to.
    workspace: Option<String>,
    today_start_ms: u64,
    last_7_days_start_ms: u64,
    last_30_days_start_ms: u64,
    heatmap_start_ms: u64,
}

impl UsageQuery {
    pub fn new(
        provider: Option<String>,
        model: Option<String>,
        workspace: Option<String>,
        today_start_ms: u64,
        last_7_days_start_ms: u64,
        last_30_days_start_ms: u64,
        heatmap_start_ms: u64,
    ) -> Self {
        Self {
            provider,
            model,
            workspace,
            today_start_ms,
            last_7_days_start_ms,
            last_30_days_start_ms,
            heatmap_start_ms,
        }
    }
}

/// Provider-returned request markers (what support and tracing ask for): the
/// request/trace ids from the response headers, the response body's id, and the
/// CDN ray. Kept together so the log row does not grow one argument per id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestLogIds {
    request_id: Option<String>,
    trace_id: Option<String>,
    response_id: Option<String>,
    cf_ray: Option<String>,
}

impl RequestLogIds {
    pub fn new(
        request_id: Option<String>,
        trace_id: Option<String>,
        response_id: Option<String>,
        cf_ray: Option<String>,
    ) -> Self {
        Self {
            request_id,
            trace_id,
            response_id,
            cf_ray,
        }
    }

    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    pub fn trace_id(&self) -> Option<&str> {
        self.trace_id.as_deref()
    }

    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    pub fn cf_ray(&self) -> Option<&str> {
        self.cf_ray.as_deref()
    }

    pub fn is_empty(&self) -> bool {
        self.request_id.is_none()
            && self.trace_id.is_none()
            && self.response_id.is_none()
            && self.cf_ray.is_none()
    }
}

/// One recorded API request (an `api_usage` row) as shown by the usage page's
/// request log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestLogRow {
    started_at_ms: u64,
    workspace: Option<String>,
    provider: String,
    model: String,
    finish: String,
    error: Option<String>,
    http_status: Option<i64>,
    ids: RequestLogIds,
    usage_reported: bool,
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
    duration_ms: u64,
    ttft_ms: Option<u64>,
}

impl RequestLogRow {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        started_at_ms: u64,
        workspace: Option<String>,
        provider: String,
        model: String,
        finish: String,
        error: Option<String>,
        http_status: Option<i64>,
        ids: RequestLogIds,
        usage_reported: bool,
        input_tokens: u64,
        cache_read_tokens: u64,
        output_tokens: u64,
        duration_ms: u64,
        ttft_ms: Option<u64>,
    ) -> Self {
        Self {
            started_at_ms,
            workspace,
            provider,
            model,
            finish,
            error,
            http_status,
            ids,
            usage_reported,
            input_tokens,
            cache_read_tokens,
            output_tokens,
            duration_ms,
            ttft_ms,
        }
    }

    pub fn started_at_ms(&self) -> u64 {
        self.started_at_ms
    }

    /// Session cwd of the call; `None` for an orphaned row from an older database.
    pub fn workspace(&self) -> Option<&str> {
        self.workspace.as_deref()
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn finish(&self) -> &str {
        &self.finish
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn http_status(&self) -> Option<i64> {
        self.http_status
    }

    /// Provider-returned request markers for debugging/support.
    pub fn ids(&self) -> &RequestLogIds {
        &self.ids
    }

    /// Whether the provider reported usage for this call (an unreported call's
    /// zero token counts are unknown, not zero).
    pub fn usage_reported(&self) -> bool {
        self.usage_reported
    }

    pub fn input_tokens(&self) -> u64 {
        self.input_tokens
    }

    pub fn cache_read_tokens(&self) -> u64 {
        self.cache_read_tokens
    }

    pub fn output_tokens(&self) -> u64 {
        self.output_tokens
    }

    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    pub fn ttft_ms(&self) -> Option<u64> {
        self.ttft_ms
    }
}

/// One page of the request log plus the filtered row count.
#[derive(Clone, Debug, Default)]
pub struct RequestLogPage {
    rows: Vec<RequestLogRow>,
    total: usize,
}

impl RequestLogPage {
    pub fn new(rows: Vec<RequestLogRow>, total: usize) -> Self {
        Self { rows, total }
    }

    pub fn rows(&self) -> &[RequestLogRow] {
        &self.rows
    }

    pub fn total(&self) -> usize {
        self.total
    }
}

/// Filters and page window of the request log (newest first).
#[derive(Clone, Debug)]
pub struct RequestLogQuery {
    provider: Option<String>,
    model: Option<String>,
    workspace: Option<String>,
    /// Zero-based page index.
    page: usize,
    page_size: usize,
}

impl RequestLogQuery {
    pub fn new(
        provider: Option<String>,
        model: Option<String>,
        workspace: Option<String>,
        page: usize,
        page_size: usize,
    ) -> Self {
        Self {
            provider,
            model,
            workspace,
            page,
            page_size,
        }
    }
}

/// The largest page the log query accepts; the UI offers 10/20/50/100/500.
const MAX_LOG_PAGE_SIZE: usize = 500;

const UNIFIED_USAGE: &str = r#"
WITH unified_usage AS (
    SELECT
        api_usage.started_at_ms,
        api_usage.session_id,
        api_usage.provider,
        api_usage.model,
        api_usage.input_tokens,
        api_usage.cache_read_tokens,
        api_usage.output_tokens,
        sessions.cwd AS workspace
    FROM api_usage
    LEFT JOIN sessions ON sessions.id = api_usage.session_id
)
"#;

const FILTERED_USAGE: &str = r#",
filtered_usage AS (
    SELECT *
    FROM unified_usage
    WHERE (?1 IS NULL OR provider = ?1)
      AND (?2 IS NULL OR model = ?2)
      AND (?3 IS NULL OR workspace = ?3)
)
"#;

fn non_negative(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

pub(crate) fn query(conn: &Connection, query: &UsageQuery) -> Result<UsageStats, String> {
    let provider = query.provider.as_deref();
    let model = query.model.as_deref();
    let workspace = query.workspace.as_deref();
    let summary_sql = format!(
        "{UNIFIED_USAGE}{FILTERED_USAGE}
         SELECT
             COALESCE(SUM(input_tokens + cache_read_tokens + output_tokens), 0),
             COALESCE(SUM(CASE WHEN started_at_ms >= ?4
                 THEN input_tokens + cache_read_tokens + output_tokens ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN started_at_ms >= ?5
                 THEN input_tokens + cache_read_tokens + output_tokens ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN started_at_ms >= ?6
                 THEN input_tokens + cache_read_tokens + output_tokens ELSE 0 END), 0)
         FROM filtered_usage"
    );
    let summary = conn
        .query_row(
            &summary_sql,
            params![
                provider,
                model,
                workspace,
                query.last_30_days_start_ms,
                query.last_7_days_start_ms,
                query.today_start_ms,
            ],
            |row| {
                Ok(UsageSummary::new(
                    non_negative(row.get(0)?),
                    non_negative(row.get(1)?),
                    non_negative(row.get(2)?),
                    non_negative(row.get(3)?),
                ))
            },
        )
        .map_err(|error| format!("Failed to query usage summary: {error}"))?;

    let daily_sql = format!(
        "{UNIFIED_USAGE}{FILTERED_USAGE}
         SELECT
             strftime('%Y-%m-%d', started_at_ms / 1000, 'unixepoch', 'localtime'),
             SUM(input_tokens + cache_read_tokens + output_tokens)
         FROM filtered_usage
         WHERE started_at_ms >= ?4
         GROUP BY 1
         ORDER BY 1"
    );
    let mut daily_stmt = conn
        .prepare(&daily_sql)
        .map_err(|error| format!("Failed to prepare daily usage query: {error}"))?;
    let daily = daily_stmt
        .query_map(
            params![provider, model, workspace, query.heatmap_start_ms],
            |row| {
                Ok(DailyUsage::new(
                    row.get::<_, String>(0)?,
                    non_negative(row.get(1)?),
                ))
            },
        )
        .map_err(|error| format!("Failed to query daily usage: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Failed to read daily usage: {error}"))?;

    let options_sql = format!(
        "{UNIFIED_USAGE}
         SELECT provider, model
         FROM unified_usage
         GROUP BY provider, model
         ORDER BY provider COLLATE NOCASE, model COLLATE NOCASE"
    );
    let mut options_stmt = conn
        .prepare(&options_sql)
        .map_err(|error| format!("Failed to prepare usage options query: {error}"))?;
    let option_rows = options_stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| format!("Failed to query usage options: {error}"))?;
    let mut providers = BTreeSet::new();
    let mut models = BTreeSet::new();
    for row in option_rows {
        let (provider, model) =
            row.map_err(|error| format!("Failed to read usage options: {error}"))?;
        providers.insert(provider);
        models.insert(model);
    }

    let workspaces_sql = format!(
        "{UNIFIED_USAGE}
         SELECT DISTINCT workspace
         FROM unified_usage
         WHERE workspace IS NOT NULL
         ORDER BY workspace COLLATE NOCASE"
    );
    let mut workspaces_stmt = conn
        .prepare(&workspaces_sql)
        .map_err(|error| format!("Failed to prepare usage workspace query: {error}"))?;
    let workspaces = workspaces_stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| format!("Failed to query usage workspaces: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Failed to read usage workspaces: {error}"))?;

    Ok(UsageStats::new(
        summary,
        providers.into_iter().collect(),
        models.into_iter().collect(),
        workspaces,
        daily,
    ))
}

/// The request log shares the usage page's provider/model/workspace filters.
const LOG_FILTER: &str = "
    WHERE (?1 IS NULL OR a.provider = ?1)
      AND (?2 IS NULL OR a.model = ?2)
      AND (?3 IS NULL OR s.cwd = ?3)";

pub(crate) fn request_log(
    conn: &Connection,
    query: &RequestLogQuery,
) -> Result<RequestLogPage, String> {
    let provider = query.provider.as_deref();
    let model = query.model.as_deref();
    let workspace = query.workspace.as_deref();
    let page_size = query.page_size.clamp(1, MAX_LOG_PAGE_SIZE);
    let offset = query.page.saturating_mul(page_size);
    let page_size = i64::try_from(page_size).unwrap_or(MAX_LOG_PAGE_SIZE as i64);
    let offset = i64::try_from(offset).unwrap_or(i64::MAX);

    let total_sql = format!(
        "SELECT COUNT(*)
         FROM api_usage AS a
         LEFT JOIN sessions AS s ON s.id = a.session_id
         {LOG_FILTER}"
    );
    let total: i64 = conn
        .query_row(&total_sql, params![provider, model, workspace], |row| {
            row.get(0)
        })
        .map_err(|error| format!("Failed to count request log: {error}"))?;

    let rows_sql = format!(
        "SELECT
             a.started_at_ms,
             s.cwd,
             a.provider,
             a.model,
             a.finish,
             a.error,
             a.http_status,
             a.request_id,
             a.trace_id,
             a.response_id,
             a.cf_ray,
             a.usage_reported,
             a.input_tokens,
             a.cache_read_tokens,
             a.output_tokens,
             a.duration_ms,
             a.ttft_ms
         FROM api_usage AS a
         LEFT JOIN sessions AS s ON s.id = a.session_id
         {LOG_FILTER}
         ORDER BY a.started_at_ms DESC, a.id DESC
         LIMIT ?4 OFFSET ?5"
    );
    let mut stmt = conn
        .prepare(&rows_sql)
        .map_err(|error| format!("Failed to prepare request log query: {error}"))?;
    let rows = stmt
        .query_map(
            params![provider, model, workspace, page_size, offset],
            |row| {
                Ok(RequestLogRow::new(
                    non_negative(row.get::<_, i64>(0)?),
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    RequestLogIds::new(
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, Option<String>>(10)?,
                    ),
                    row.get::<_, bool>(11)?,
                    non_negative(row.get::<_, i64>(12)?),
                    non_negative(row.get::<_, i64>(13)?),
                    non_negative(row.get::<_, i64>(14)?),
                    non_negative(row.get::<_, i64>(15)?),
                    row.get::<_, Option<i64>>(16)?.map(non_negative),
                ))
            },
        )
        .map_err(|error| format!("Failed to query request log: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Failed to read request log: {error}"))?;

    Ok(RequestLogPage::new(
        rows,
        usize::try_from(total).unwrap_or(0),
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pig_provider::ProviderUsage;

    use super::*;
    use crate::api_usage::ApiUsageRecord;
    use crate::store::Store;

    fn open_test_store(name: &str) -> (PathBuf, Store) {
        let dir =
            std::env::temp_dir().join(format!("pig-core-usage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let store = Store::open(&dir).expect("open store");
        (dir, store)
    }

    #[allow(clippy::too_many_arguments)]
    fn record_api(
        store: &Store,
        session_id: &str,
        source: &str,
        started_at_ms: u64,
        provider: &str,
        model: &str,
        input: u64,
        cache_read: u64,
        output: u64,
    ) {
        let mut record = ApiUsageRecord {
            session_id: session_id.to_string(),
            source: source.to_string(),
            started_at_ms,
            provider: provider.to_string(),
            model: model.to_string(),
            ..Default::default()
        };
        record.set_usage(Some(ProviderUsage {
            input,
            cache_read,
            output,
            used: 999_999,
            total: 1_000_000,
            reasoning_output: output,
        }));
        store.record_api_usage(&record);
    }

    #[test]
    fn per_call_rows_aggregate_by_provider_and_model() {
        let (_dir, store) = open_test_store("aggregate");
        record_api(&store, "s1", "main", 1_000_000, "p1", "m-old", 1, 2, 3);
        record_api(&store, "s2", "main", 1_500_000, "p2", "m-other", 5, 0, 5);
        record_api(&store, "s1", "main", 1_500_000, "p1", "m-new", 10, 20, 30);
        record_api(&store, "s1", "subagent", 1_600_000, "p1", "m-new", 1, 2, 3);
        record_api(&store, "s1", "compact", 1_700_000, "p1", "m-new", 2, 3, 4);
        record_api(&store, "s1", "title", 1_800_000, "p1", "m-new", 1, 1, 1);

        let stats = store
            .usage_stats(&UsageQuery::new(None, None, None, 0, 0, 0, 0))
            .expect("usage query");
        // 6 m-old + 10 from the other session + 60 main + 6 subagent + 9 compact + 3 title.
        assert_eq!(stats.summary().total(), 94);
        assert_eq!(stats.providers(), &["p1", "p2"]);
        assert_eq!(stats.models(), &["m-new", "m-old", "m-other"]);

        let filtered = store
            .usage_stats(&UsageQuery::new(
                Some("p1".to_string()),
                Some("m-new".to_string()),
                None,
                0,
                0,
                0,
                0,
            ))
            .expect("filtered usage query");
        assert_eq!(filtered.summary().total(), 78);
    }

    #[test]
    fn summary_ranges_and_daily_rows_follow_local_calendar() {
        let (_dir, store) = open_test_store("ranges");
        let now =
            time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        let today_start = now
            .date()
            .with_time(time::Time::MIDNIGHT)
            .assume_offset(now.offset())
            .unix_timestamp() as u64
            * 1000;
        let day_ms = 86_400_000;
        let last_7_start = today_start - 6 * day_ms;
        let last_30_start = today_start - 29 * day_ms;
        for (offset_days, input) in [(31, 1), (29, 2), (6, 4)] {
            record_api(
                &store,
                "s1",
                "main",
                today_start - offset_days * day_ms,
                "p",
                "m",
                input,
                1,
                2,
            );
        }
        record_api(&store, "s1", "main", today_start + 60_000, "p", "m", 8, 1, 2);
        record_api(
            &store,
            "s1",
            "main",
            today_start + 120_000,
            "other",
            "m",
            100,
            0,
            0,
        );

        let stats = store
            .usage_stats(&UsageQuery::new(
                Some("p".to_string()),
                Some("m".to_string()),
                None,
                today_start,
                last_7_start,
                last_30_start,
                last_30_start,
            ))
            .expect("usage query");
        assert_eq!(stats.summary().total(), 27);
        assert_eq!(stats.summary().last_30_days(), 23);
        assert_eq!(stats.summary().last_7_days(), 18);
        assert_eq!(stats.summary().today(), 11);

        let today = format!(
            "{:04}-{:02}-{:02}",
            now.year(),
            u8::from(now.month()),
            now.day()
        );
        let today_usage = stats
            .daily()
            .iter()
            .find(|row| row.day() == today)
            .expect("today's daily row");
        assert_eq!(today_usage.tokens(), 11);
    }

    fn session_meta(id: &str, cwd: &str) -> pig_protocol::SessionMeta {
        pig_protocol::SessionMeta {
            id: id.to_string(),
            title: id.to_string(),
            cwd: PathBuf::from(cwd),
            created_at: 1,
            updated_at: 1,
            pinned: false,
            archived: false,
            title_custom: false,
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            permissions: Default::default(),
            plan_enabled: false,
        }
    }

    #[test]
    fn workspace_filter_narrows_every_total_and_offers_all_workspaces() {
        let (_dir, store) = open_test_store("workspace");
        store.upsert_session(&session_meta("s1", "/work/a"));
        store.upsert_session(&session_meta("s2", "/work/b"));
        record_api(&store, "s1", "main", 1_000, "p1", "m1", 10, 0, 10);
        record_api(&store, "s2", "main", 2_000, "p2", "m2", 5, 0, 5);
        // Usage without a session row never matches a workspace.
        record_api(&store, "orphan", "main", 3_000, "p3", "m3", 7, 0, 7);

        let all = store
            .usage_stats(&UsageQuery::new(None, None, None, 0, 0, 0, 0))
            .expect("usage query");
        assert_eq!(all.summary().total(), 44);
        assert_eq!(all.workspaces(), &["/work/a", "/work/b"]);

        let filtered = store
            .usage_stats(&UsageQuery::new(
                None,
                None,
                Some("/work/a".to_string()),
                0,
                0,
                0,
                0,
            ))
            .expect("workspace usage query");
        assert_eq!(filtered.summary().total(), 20);
        assert_eq!(
            filtered.daily().iter().map(DailyUsage::tokens).sum::<u64>(),
            20,
            "the heatmap rows follow the workspace filter"
        );
        assert_eq!(
            filtered.workspaces(),
            &["/work/a", "/work/b"],
            "filter choices stay global so another workspace remains selectable"
        );

        let combined = store
            .usage_stats(&UsageQuery::new(
                Some("p1".to_string()),
                None,
                Some("/work/b".to_string()),
                0,
                0,
                0,
                0,
            ))
            .expect("combined usage query");
        assert_eq!(combined.summary().total(), 0);
    }

    fn record_log(
        store: &Store,
        session_id: &str,
        started_at_ms: u64,
        provider: &str,
        model: &str,
    ) {
        store.record_api_usage(&ApiUsageRecord {
            session_id: session_id.to_string(),
            source: "main".to_string(),
            started_at_ms,
            provider: provider.to_string(),
            model: model.to_string(),
            duration_ms: 1_500,
            ttft_ms: Some(300),
            usage_reported: true,
            input_tokens: 10,
            cache_read_tokens: 20,
            output_tokens: 30,
            finish: "stop".to_string(),
            response: pig_provider::ProviderResponseMetadata {
                http_status: Some(200),
                request_id: Some(format!("req-{started_at_ms}")),
                trace_id: Some(format!("trace-{started_at_ms}")),
                response_id: Some(format!("resp-{started_at_ms}")),
                cf_ray: Some(format!("ray-{started_at_ms}")),
            },
            ..Default::default()
        });
    }

    fn log_times(page: &RequestLogPage) -> Vec<u64> {
        page.rows()
            .iter()
            .map(RequestLogRow::started_at_ms)
            .collect()
    }

    #[test]
    fn request_log_pages_newest_first_and_shares_usage_filters() {
        let (_dir, store) = open_test_store("request-log");
        store.upsert_session(&session_meta("s1", "/work/a"));
        store.upsert_session(&session_meta("s2", "/work/b"));
        for (started, session, provider, model) in [
            (1_000, "s1", "p1", "m1"),
            (2_000, "s2", "p2", "m2"),
            (3_000, "s1", "p1", "m1"),
            (4_000, "s2", "p2", "m2"),
            (5_000, "s1", "p1", "m1"),
            // Same millisecond as the previous row: the later insert sorts first.
            (5_000, "s1", "p1", "m-late"),
        ] {
            record_log(&store, session, started, provider, model);
        }
        // Orphaned usage (no session row) still appears in the unfiltered log.
        record_log(&store, "orphan", 6_000, "p3", "m3");

        let first = store
            .request_log(&RequestLogQuery::new(None, None, None, 0, 2))
            .expect("first request log page");
        assert_eq!(first.total(), 7);
        assert_eq!(log_times(&first), [6_000, 5_000]);
        assert_eq!(first.rows()[0].model(), "m3");
        assert_eq!(first.rows()[0].workspace(), None);
        assert_eq!(first.rows()[1].model(), "m-late");
        assert_eq!(first.rows()[1].workspace(), Some("/work/a"));

        let second = store
            .request_log(&RequestLogQuery::new(None, None, None, 1, 2))
            .expect("second request log page");
        assert_eq!(log_times(&second), [5_000, 4_000]);
        assert_eq!(second.rows()[0].model(), "m1");
        let row = &second.rows()[0];
        assert_eq!(row.provider(), "p1");
        assert_eq!(row.finish(), "stop");
        assert_eq!(row.error(), None);
        assert_eq!(row.http_status(), Some(200));
        assert_eq!(row.ids().request_id(), Some("req-5000"));
        assert_eq!(row.ids().trace_id(), Some("trace-5000"));
        assert_eq!(row.ids().response_id(), Some("resp-5000"));
        assert_eq!(row.ids().cf_ray(), Some("ray-5000"));
        assert!(row.usage_reported());
        assert_eq!(row.input_tokens(), 10);
        assert_eq!(row.cache_read_tokens(), 20);
        assert_eq!(row.output_tokens(), 30);
        assert_eq!(row.duration_ms(), 1_500);
        assert_eq!(row.ttft_ms(), Some(300));

        let last = store
            .request_log(&RequestLogQuery::new(None, None, None, 3, 2))
            .expect("last request log page");
        assert_eq!(log_times(&last), [1_000]);

        let workspace = store
            .request_log(&RequestLogQuery::new(
                None,
                None,
                Some("/work/a".to_string()),
                0,
                2,
            ))
            .expect("workspace request log page");
        assert_eq!(workspace.total(), 4);
        assert_eq!(log_times(&workspace), [5_000, 5_000]);
        assert!(
            workspace
                .rows()
                .iter()
                .all(|row| row.workspace() == Some("/work/a"))
        );

        let no_match = store
            .request_log(&RequestLogQuery::new(
                Some("p1".to_string()),
                None,
                Some("/work/b".to_string()),
                0,
                10,
            ))
            .expect("mismatched request log query");
        assert_eq!(no_match.total(), 0);
        assert!(no_match.rows().is_empty());
    }
}
