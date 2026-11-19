// SPDX-License-Identifier: Apache-2.0
//
// Adapted from OpenAI Codex's codex-rs/thread-manager-sample/src/main.rs
// (Apache-2.0, Copyright 2025 OpenAI) at the revision in deps/codex.toml.

//! Running atoms on Codex: the entry point and one thread turn.
//!
//! In v0.1 atomic drives Codex threads through `ThreadManager` directly. The
//! `Config` below is built in full, as Codex's own sample does, so nothing is
//! read from the user's Codex configuration: Codex's home is the project's
//! state directory (see [`crate::state`]).

use crate::usage::{Call, Metering, Outcome, Usage, meter_error};
use anyhow::Context;
use anyhow::bail;
use codex_core_api::AbsolutePathBuf;
use codex_core_api::AltScreenMode;
use codex_core_api::ApprovalsReviewer;
use codex_core_api::Arg0DispatchPaths;
use codex_core_api::AskForApproval;
use codex_core_api::AuthKeyringBackendKind;
use codex_core_api::AuthManager;
use codex_core_api::AutoCompactTokenLimitScope;
use codex_core_api::CodexAppsToolsCache;
use codex_core_api::CodexHomeUserInstructionsProvider;
use codex_core_api::CodexThread;
use codex_core_api::Config;
use codex_core_api::ConfigLayerStack;
use codex_core_api::ConfigLoadOptions;
use codex_core_api::ConfigRequirements;
use codex_core_api::ConfigRequirementsToml;
use codex_core_api::Constrained;
use codex_core_api::EnvironmentManager;
use codex_core_api::EventMsg;
use codex_core_api::ExecServerRuntimeOptions;
use codex_core_api::ExtensionRegistryBuilder;
use codex_core_api::Feature;
use codex_core_api::Features;
use codex_core_api::GhostSnapshotConfig;
use codex_core_api::History;
use codex_core_api::MemoriesConfig;
use codex_core_api::ModelAvailabilityNuxConfig;
use codex_core_api::ModelCatalogSettings;
use codex_core_api::MultiAgentV2Config;
use codex_core_api::NewThread;
use codex_core_api::NotSubmittedReason;
use codex_core_api::Notice;
use codex_core_api::OPENAI_PROVIDER_ID;
use codex_core_api::Op;
use codex_core_api::OtelConfig;
use codex_core_api::PermissionProfile;
use codex_core_api::Permissions;
use codex_core_api::ProjectConfig;
use codex_core_api::RealtimeAudioConfig;
use codex_core_api::RealtimeConfig;
use codex_core_api::SessionPickerViewMode;
use codex_core_api::SessionSource;
use codex_core_api::SqliteConfig;
use codex_core_api::StartIfIdleSubmission;
use codex_core_api::StartThreadOptions;
use codex_core_api::TerminalResizeReflowConfig;
use codex_core_api::ThreadManager;
use codex_core_api::ThreadStoreBuilder;
use codex_core_api::ThreadStoreConfig;
use codex_core_api::ToolSuggestConfig;
use codex_core_api::TuiKeymap;
use codex_core_api::TuiNotificationSettings;
use codex_core_api::TuiPetAnchor;
use codex_core_api::TurnInputRequest;
use codex_core_api::UriBasedFileOpener;
use codex_core_api::UserInput;
use codex_core_api::WebSearchMode;
use codex_core_api::arg0_dispatch_or_else;
use codex_core_api::bootstrap_auth_config;
use codex_core_api::built_in_model_providers;
use codex_core_api::init_state_db;
use codex_core_api::install_image_generation_extension;
use codex_core_api::load_config_toml_with_layer_stack;
use codex_core_api::local_agent_graph_store_from_state_db;
use codex_core_api::passthrough_image_store;
use codex_core_api::resolve_bootstrap_respect_system_proxy;
use codex_core_api::resolve_installation_id;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The provider id atomic registers for its model endpoint.
const ENDPOINT_PROVIDER_ID: &str = "atomic-endpoint";

/// An OpenAI-compatible Responses API endpoint and the model to use there.
#[derive(Clone, Debug)]
pub struct ModelEndpoint {
    /// Base URL of the API, for example `http://127.0.0.1:13305/api/v1`.
    pub base_url: String,
    /// Model id at that endpoint.
    pub model: String,
    /// Environment variable holding the API key, if the endpoint needs one.
    pub api_key_env: Option<String>,
}

/// One turn to run on a fresh thread.
#[derive(Clone, Debug, Default)]
pub struct TurnSpec {
    /// Instructions for the developer role; never holds input data.
    pub developer_instructions: Option<String>,
    /// The user turn.
    pub prompt: String,
    /// A JSON Schema the final answer must follow (structured output).
    pub output_schema: Option<serde_json::Value>,
    /// A model name that overrides the endpoint's.
    pub model: Option<String>,
    /// The sandbox the thread runs in. The network is always restricted.
    pub sandbox: crate::typed::Sandbox,
    /// Environment variables set for every command the thread runs, such as
    /// the project's shared caches (see [`crate::worktree::Caches`]).
    pub env: BTreeMap<String, String>,
    /// Directories a `workspace-write` thread may write besides its working
    /// directory, such as the project's shared caches. Absolute paths.
    pub writable_roots: Vec<PathBuf>,
}

/// Codex's permission profile for a sandbox: read-only, or writes confined
/// to the workspace; the network is restricted either way.
pub(crate) fn permission_profile(sandbox: crate::typed::Sandbox) -> PermissionProfile {
    match sandbox {
        crate::typed::Sandbox::ReadOnly => PermissionProfile::read_only(),
        crate::typed::Sandbox::WorkspaceWrite => PermissionProfile::workspace_write(),
    }
}

/// What a thread did: its id, how many turns it ran and the tokens it used,
/// as the provider reported them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadReport {
    /// Codex's id for the thread.
    pub thread_id: String,
    /// Turns run on it.
    pub turns: u32,
    /// Input tokens, over all turns.
    pub input_tokens: i64,
    /// Output tokens, over all turns.
    pub output_tokens: i64,
    /// Total tokens, over all turns.
    pub total_tokens: i64,
}

/// Paths Codex needs to re-run this executable as one of its helpers.
#[derive(Clone, Debug)]
pub struct CodexPaths(Arg0DispatchPaths);

impl CodexPaths {
    /// Paths for a process that did not start through [`dispatch`], such as
    /// a test: Codex can re-run this executable, but has no sandbox helper.
    pub fn without_helpers() -> Self {
        Self(Arg0DispatchPaths {
            codex_self_exe: std::env::current_exe().ok(),
            ..Arg0DispatchPaths::default()
        })
    }

    /// Paths for a process that did not start through [`dispatch`] but has
    /// atomic's binary at hand, such as a test: Codex re-runs `atomic` for
    /// its helpers, and runs `linux_sandbox`, a link to `atomic` named
    /// `codex-linux-sandbox`, as its Linux sandbox.
    pub fn with_helpers(atomic: PathBuf, linux_sandbox: PathBuf) -> Self {
        Self(Arg0DispatchPaths {
            codex_self_exe: Some(atomic),
            codex_linux_sandbox_exe: Some(linux_sandbox),
            ..Arg0DispatchPaths::default()
        })
    }
}

/// The process entry point.
///
/// The same executable re-runs itself as Codex's helpers (the Linux sandbox,
/// `apply_patch`); those invocations are dispatched here and never reach
/// `main_fn`. Before anything else, `CODEX_HOME` is pointed at `codex_home`, so
/// Codex never creates or reads files in the user's own Codex home. Call this
/// first in `main`, before any thread starts.
pub fn dispatch<F, Fut>(codex_home: &Path, main_fn: F) -> anyhow::Result<()>
where
    F: FnOnce(CodexPaths) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>>,
{
    // SAFETY: called at the start of main, while the process has one thread,
    // so no other thread can read the environment concurrently.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("CODEX_HOME", codex_home);
    }
    arg0_dispatch_or_else(move |paths| main_fn(CodexPaths(paths)))
}

/// Run one turn on a fresh, ephemeral Codex thread and return the model's
/// final message. The thread runs with read-only permissions and never asks
/// for approval.
pub async fn one_turn(
    paths: &CodexPaths,
    codex_home: &Path,
    cwd: &Path,
    endpoint: &ModelEndpoint,
    prompt: &str,
) -> anyhow::Result<String> {
    let spec = TurnSpec {
        prompt: prompt.to_owned(),
        ..TurnSpec::default()
    };
    run(paths, codex_home, cwd, endpoint, &spec).await
}

/// Run `spec` as one turn on a fresh, ephemeral thread and return the final
/// message.
pub async fn run(
    paths: &CodexPaths,
    codex_home: &Path,
    cwd: &Path,
    endpoint: &ModelEndpoint,
    spec: &TurnSpec,
) -> anyhow::Result<String> {
    converse(paths, codex_home, cwd, endpoint, spec, |_| None)
        .await
        .map(|(answer, _)| answer)
}

/// Run `first` on a fresh, ephemeral thread, then, after each answer, the
/// follow-up turn `next` returns on the same thread, until it returns `None`.
/// Returns the last answer and how many turns ran. Follow-up turns use the
/// thread's instructions and model; their own are ignored.
pub async fn converse<F>(
    paths: &CodexPaths,
    codex_home: &Path,
    cwd: &Path,
    endpoint: &ModelEndpoint,
    first: &TurnSpec,
    mut next: F,
) -> anyhow::Result<(String, ThreadReport)>
where
    F: FnMut(&str) -> Option<TurnSpec>,
{
    converse_async(
        paths,
        codex_home,
        cwd,
        endpoint,
        &Metering::none(),
        first,
        |answer| std::future::ready(next(&answer)),
    )
    .await
}

/// Conversation with asynchronous checks between turns. Each turn is
/// admitted by `meter` before it is submitted and settled once after it.
pub async fn converse_async<F, Fut>(
    paths: &CodexPaths,
    codex_home: &Path,
    cwd: &Path,
    endpoint: &ModelEndpoint,
    meter: &Metering,
    first: &TurnSpec,
    mut next: F,
) -> anyhow::Result<(String, ThreadReport)>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Option<TurnSpec>>,
{
    let spec = first;
    let mut endpoint = endpoint.clone();
    if let Some(m) = &spec.model {
        endpoint.model = m.clone();
    }
    let mut config = new_config(&endpoint, codex_home, cwd, paths.0.clone()).await?;
    config.developer_instructions = spec.developer_instructions.clone();
    config.permissions = Permissions::from_approval_and_profile(
        Constrained::allow_any(AskForApproval::Never),
        Constrained::allow_any(permission_profile(spec.sandbox)),
    )?;
    if !spec.writable_roots.is_empty() {
        let mut roots = vec![config.cwd.clone()];
        for root in &spec.writable_roots {
            let root = AbsolutePathBuf::from_absolute_path(root)
                .with_context(|| format!("resolve the writable root {}", root.display()))?;
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        config.permissions.set_workspace_roots(roots.clone());
        config.workspace_roots = roots;
        config.workspace_roots_explicit = true;
    }
    for (name, value) in &spec.env {
        config
            .permissions
            .shell_environment_policy
            .r#set
            .insert(name.clone(), value.clone());
    }
    let state_db = init_state_db(&config).await;

    let auth_manager =
        AuthManager::shared_from_config(&config, /*enable_codex_api_key_env*/ false).await?;
    let local_runtime_paths = ExecServerRuntimeOptions::from_optional_paths(
        config.codex_self_exe.clone(),
        config.codex_linux_sandbox_exe.clone(),
    )?;
    let thread_store = ThreadStoreBuilder::from_config(&config, state_db.clone()).build();
    let environment_manager = Arc::new(
        EnvironmentManager::from_codex_home(
            config.codex_home.clone(),
            Some(local_runtime_paths),
            config.http_client_factory(),
        )
        .await?,
    );
    let installation_id = resolve_installation_id(&config.codex_home).await?;
    let user_instructions_provider = Arc::new(CodexHomeUserInstructionsProvider::new(
        config.codex_home.clone(),
    ));
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    install_image_generation_extension(&mut extensions, auth_manager.clone(), |config: &Config| {
        Some(config.codex_home.clone())
    });
    let startup = codex_core_api::build_startup_models(
        &config,
        Arc::clone(&auth_manager),
        installation_id,
        Arc::new(ModelCatalogSettings::disk(config.codex_home.to_path_buf())),
    );
    let thread_manager = ThreadManager::new(
        &config,
        startup,
        CodexAppsToolsCache::default(),
        SessionSource::Exec,
        environment_manager,
        Arc::new(extensions.build()),
        user_instructions_provider,
        /*analytics_events_client*/ None,
        passthrough_image_store(),
        Arc::clone(&thread_store),
        local_agent_graph_store_from_state_db(state_db.as_ref()),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );

    let NewThread {
        thread_id, thread, ..
    } = thread_manager
        .start_thread(StartThreadOptions::new(config))
        .await
        .context("start Codex thread")?;

    let mut current = spec.clone();
    let mut report = ThreadReport {
        thread_id: thread_id.to_string(),
        ..ThreadReport::default()
    };
    // The thread's running totals as last reported: each turn's usage is
    // what they grew by, so each reported token is charged once.
    let mut seen = (0_i64, 0_i64);
    let outcome = loop {
        // Admitted before the turn is submitted, settled once after it.
        let (ticket, limit) = match meter
            .admit(Call {
                index: report.turns,
                model: endpoint.model.clone(),
                input_bound: None,
            })
            .await
        {
            Ok(admitted) => admitted,
            Err(e) => break Err(meter_error(e)),
        };
        report.turns += 1;
        // A turn is cut off when its reserved time runs out; a failed or cut
        // off turn still reports the usage seen before it stopped.
        let (turn, totals) = run_turn(&thread, &current, limit.time).await;
        // Codex repeats the last totals in every token count, so a turn
        // whose requests reported nothing shows them unchanged; a request
        // that reports usage always grows them. A turn with nothing reported
        // spent something unknown and stays charged at its reservation.
        let fresh = totals.filter(|(input, output, _)| *input > seen.0 || *output > seen.1);
        let outcome = match fresh {
            Some((input, output, total)) => {
                let grown = Usage {
                    input_tokens: (input - seen.0).max(0),
                    output_tokens: (output - seen.1).max(0),
                };
                seen = (input, output);
                report.input_tokens = input;
                report.output_tokens = output;
                report.total_tokens = total;
                Outcome::Reported(grown)
            }
            None => Outcome::Unknown,
        };
        if let Err(e) = meter.settle(ticket, outcome).await {
            break Err(meter_error(e));
        }
        match turn {
            Err(e) => break Err(e),
            Ok(answer) => match next(answer.clone()).await {
                Some(follow_up) => current = follow_up,
                None => break Ok((answer, report.clone())),
            },
        }
    };
    let shutdown_result = thread.shutdown_and_wait().await;
    let _ = thread_manager.remove_thread(&thread_id).await;

    let outcome = outcome?;
    shutdown_result.context("shut down Codex thread")?;
    Ok(outcome)
}

async fn new_config(
    endpoint: &ModelEndpoint,
    codex_home: &Path,
    cwd: &Path,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<Config> {
    let codex_home =
        AbsolutePathBuf::from_absolute_path(codex_home).context("resolve the Codex home")?;
    let cwd = AbsolutePathBuf::from_absolute_path(cwd).context("resolve the working directory")?;
    let model = Some(endpoint.model.clone());
    // Resolve auth through the shared bootstrap path so cargo-run builds retain the configured
    // secure store instead of the full Config loader's local-development file-store override.
    let bootstrap = load_config_toml_with_layer_stack(
        codex_home.as_path(),
        // No project layer: atom threads never load the project's own Codex
        // configuration.
        None,
        Vec::new(),
        ConfigLoadOptions::default(),
    )
    .await
    .context("load authentication configuration")?;
    let auth_config = bootstrap_auth_config(codex_home.as_path(), &bootstrap)?;
    let requirements = bootstrap.config_layer_stack.requirements();
    let requirements_toml = bootstrap.config_layer_stack.requirements_toml();
    // Preserve auth restrictions for every manager rebuilt from Config without importing
    // execution settings, plugins, or hooks from the bootstrap configuration.
    let config_layer_stack = ConfigLayerStack::new(
        Vec::new(),
        ConfigRequirements {
            allowed_login_methods: requirements.allowed_login_methods.clone(),
            allowed_chatgpt_workspaces: requirements.allowed_chatgpt_workspaces.clone(),
            ..Default::default()
        },
        ConfigRequirementsToml {
            allowed_login_methods: requirements_toml.allowed_login_methods.clone(),
            allowed_chatgpt_workspaces: requirements_toml.allowed_chatgpt_workspaces.clone(),
            ..Default::default()
        },
    )?;
    let mcp_oauth_credentials_store_mode = bootstrap
        .config_toml
        .mcp_oauth_credentials_store
        .unwrap_or_default();
    let respect_system_proxy = resolve_bootstrap_respect_system_proxy(
        &bootstrap.config_toml,
        bootstrap
            .config_layer_stack
            .requirements()
            .feature_requirements
            .as_ref(),
    )?;
    // An OpenAI-compatible Responses endpoint: the built-in OpenAI provider,
    // pointed at another base URL, with no OpenAI sign-in and no websockets.
    let model_provider_id = ENDPOINT_PROVIDER_ID.to_string();
    let mut model_providers = built_in_model_providers(/*openai_base_url*/ None);
    let mut model_provider = model_providers
        .get(OPENAI_PROVIDER_ID)
        .context("OpenAI model provider should be available")?
        .clone();
    model_provider.name = "atomic endpoint".to_string();
    model_provider.base_url = Some(endpoint.base_url.clone());
    model_provider.env_key = endpoint.api_key_env.clone();
    model_provider.requires_openai_auth = false;
    model_provider.supports_websockets = false;
    model_providers.insert(model_provider_id.clone(), model_provider.clone());

    let mut config = Config {
        application_network_policy: Default::default(),
        application_auth_route_config: Some(auth_config.auth_route_config.clone()),
        config_layer_stack,
        plugins: Default::default(),
        startup_warnings: Vec::new(),
        bypass_hook_trust: false,
        model,
        daybreak_enabled: false,
        service_tier: None,
        review_model: None,
        model_context_window: None,
        model_auto_compact_token_limit: None,
        model_auto_compact_token_limit_scope: AutoCompactTokenLimitScope::Total,
        model_post_turn_compact_threshold_percent: 0,
        async_compaction_threshold_percent: 65,
        model_provider_id,
        model_provider,
        personality: None,
        permissions: Permissions::from_approval_and_profile(
            Constrained::allow_any(AskForApproval::Never),
            Constrained::allow_any(PermissionProfile::read_only()),
        )?,
        explicit_permission_profile_mode: false,
        custom_permission_profiles: Vec::new(),
        approvals_reviewer: ApprovalsReviewer::User,
        requested_approvals_reviewer: None,
        enforce_residency: Constrained::allow_any(/*initial_value*/ None),
        hide_agent_reasoning: false,
        show_raw_agent_reasoning: false,
        base_instructions: None,
        base_instructions_provenance: None,
        developer_instructions: None,
        guardian_policy_revision: 0,
        guardian_policy_config: None,
        guardian_extra_policy: None,
        guardian_policy_template: None,
        guardian_transcript_mode: Default::default(),
        guardian_sync_elastic_budget: None,
        guardian_conversation_history_prompt: None,
        guardian_conversation_history_max_output_tokens: None,
        guardian_circuit_break_action: Default::default(),
        guardian_review_timeout: Some(std::time::Duration::from_secs(90)),
        include_permissions_instructions: false,
        include_apps_instructions: false,
        include_collaboration_mode_instructions: false,
        include_skill_instructions: false,
        skill_max_context_tokens: None,
        cloud_skill_enabled: false,
        orchestrator_mcp_enabled: false,
        include_environment_context: false,
        include_environment_context_time: true,
        compact_prompt: None,
        notify: None,
        tui_notifications: TuiNotificationSettings::default(),
        animations: true,
        tui_effects: Default::default(),
        tui_rendering: Default::default(),
        show_tooltips: true,
        tui_show_server_version_notice: true,
        tui_auto_recap: true,
        model_availability_nux: ModelAvailabilityNuxConfig::default(),
        tui_fullscreen_transcript: false,
        tui_collapse_turn_details: false,
        tui_sidebar_width_percent: None,
        tui_mouse_scroll_speed: None,
        tui_copy_on_select: Default::default(),
        tui_copy_on_select_clear_selection: false,
        tui_right_click_paste: Default::default(),
        tui_alternate_screen: AltScreenMode::Auto,
        tui_status_line: None,
        tui_status_line_use_colors: true,
        tui_terminal_title: None,
        tui_theme: None,
        tui_raw_output_mode: false,
        tui_pet: None,
        tui_pet_anchor: TuiPetAnchor::Composer,
        terminal_resize_reflow: TerminalResizeReflowConfig::default(),
        tui_keymap: TuiKeymap::default(),
        tui_quick_dials: BTreeMap::new(),
        tui_session_picker_view: SessionPickerViewMode::Dense,
        tui_agents_overview_grouping: Default::default(),
        tui_resume_cwd: None,
        tui_vim_mode_default: false,
        tui_question_esc_back: true,
        cwd: cwd.clone(),
        workspace_roots: vec![cwd],
        workspace_roots_explicit: false,
        cli_auth_credentials_store_mode: auth_config.auth_credentials_store_mode,
        mcp_servers: Constrained::allow_any(HashMap::new()),
        mcp_enterprise_managed_auth: None,
        non_prefixed_mcp_tool_servers: None,
        mcp_oauth_credentials_store_mode,
        mcp_oauth_callback_port: None,
        mcp_oauth_callback_url: None,
        mcp_optional_startup_grace: std::time::Duration::from_secs(1),
        model_providers,
        project_doc_max_bytes: 32 * 1024,
        project_doc_fallback_filenames: Vec::new(),
        tool_output_token_limit: None,
        agents_enabled: true,
        agent_max_threads: Some(6),
        agent_default_subagent_model: None,
        agent_default_subagent_reasoning_effort: None,
        agent_interrupt_message_enabled: false,
        agent_max_depth: 1,
        agent_roles: BTreeMap::new(),
        memories: MemoriesConfig::default(),
        sqlite: SqliteConfig::from_sqlite_home_with_nfs_enforcement(
            codex_home.clone(),
            /*enforce_delete_on_nfs*/ false,
        ),
        log_dir: codex_home.join("log").to_path_buf(),
        codex_home,
        history: History::default(),
        ephemeral: true,
        allow_persistent_dynamic_tool_updates: false,
        extra_config: None,
        file_opener: UriBasedFileOpener::VsCode,
        codex_self_exe: arg0_paths.codex_self_exe,
        codex_linux_sandbox_exe: arg0_paths.codex_linux_sandbox_exe,
        main_execve_wrapper_exe: None,
        zsh_path: None,
        model_reasoning_effort: None,
        plan_mode_reasoning_effort: None,
        model_reasoning_summary: None,
        model_catalog: None,
        model_verbosity: None,
        chatgpt_base_url: auth_config
            .chatgpt_base_url
            .clone()
            .unwrap_or_else(|| "https://chatgpt.com/backend-api/".to_string()),
        chatgpt_base_url_is_explicit: auth_config.chatgpt_base_url.is_some(),
        respect_system_proxy,
        apps_mcp_product_sku: None,
        responses_api_metadata: BTreeMap::new(),
        realtime_audio: RealtimeAudioConfig::default(),
        experimental_realtime_ws_base_url: None,
        experimental_realtime_webrtc_call_base_url: None,
        experimental_realtime_ws_model: None,
        realtime: RealtimeConfig::default(),
        experimental_realtime_ws_backend_prompt: None,
        experimental_realtime_ws_startup_context: None,
        experimental_realtime_start_instructions: None,
        experimental_thread_store: ThreadStoreConfig::Local,
        forced_chatgpt_workspace_id: auth_config.forced_chatgpt_workspace_id.clone(),
        forced_login_method: auth_config.forced_login_method,
        web_search_mode: Constrained::allow_any(WebSearchMode::Disabled),
        web_search_config: None,
        experimental_request_user_input_enabled: true,
        update_plan_enabled: true,
        tool_registry: Default::default(),
        code_mode: Default::default(),
        background_terminal_max_timeout: 300_000,
        thread_unload_delay: std::time::Duration::from_secs(/*secs*/ 60),
        ghost_snapshot: GhostSnapshotConfig::default(),
        multi_agent_v2: MultiAgentV2Config::default(),
        max_goal_token_budget: None,
        token_budget: None,
        token_budget_startup_config: None,
        rollout_budget: None,
        current_time_reminder: None,
        sleep_tool_mode: Default::default(),
        runtime_auth_mode: Default::default(),
        features: Default::default(),
        prefer_mxc: false,

        runtime_feature_defaults: Default::default(),
        suppress_unstable_features_warning: false,
        active_project: ProjectConfig { trust_level: None },
        notices: Notice::default(),
        check_for_update_on_startup: false,
        disable_paste_burst: false,
        analytics_enabled: Some(false),
        feedback_enabled: false,
        tool_suggest: ToolSuggestConfig::default(),
        otel: OtelConfig::default(),
    };
    config
        .features
        .set(Features::with_defaults())
        .context("configure default features")?;
    // AuthManager and MCP both derive their credential backend from Config.
    config.features.set_enabled(
        Feature::SecretAuthStorage,
        auth_config.keyring_backend_kind == AuthKeyringBackendKind::Secrets,
    )?;
    Ok(config)
}

/// How long a turn waits for the thread to finish settling the previous one.
const NOT_IDLE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

/// Run one turn; return its final message and the thread's token totals
/// (input, output, total) as last reported.
/// How long an interrupted turn has to wind down.
const INTERRUPT_PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

/// Run one turn, cut off after `time` if one is given. Returns how it ended
/// and the thread's running totals (input, output, total) as last reported,
/// whether or not it succeeded.
async fn run_turn(
    thread: &CodexThread,
    spec: &TurnSpec,
    time: Option<std::time::Duration>,
) -> (anyhow::Result<String>, Option<(i64, i64, i64)>) {
    let mut usage = None;
    let ended = match time {
        None => turn(thread, spec, &mut usage).await,
        Some(time) => match tokio::time::timeout(time, turn(thread, spec, &mut usage)).await {
            Ok(ended) => ended,
            Err(_) => {
                interrupt(thread, &mut usage).await;
                Err(anyhow::anyhow!(
                    "the turn was cut off after {} ms, its reserved time",
                    time.as_millis()
                ))
            }
        },
    };
    (ended, usage)
}

/// Interrupt the running turn and wait, briefly, for it to stop, keeping
/// any usage it reports on the way.
async fn interrupt(thread: &CodexThread, usage: &mut Option<(i64, i64, i64)>) {
    if thread.submit(Op::Interrupt).await.is_err() {
        return;
    }
    let _ = tokio::time::timeout(INTERRUPT_PATIENCE, async {
        while let Ok(event) = thread.next_event().await {
            match event.msg {
                EventMsg::TokenCount(count) => {
                    if let Some(info) = count.info {
                        let t = info.total_token_usage;
                        *usage = Some((t.input_tokens, t.output_tokens, t.total_tokens));
                    }
                }
                EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_) => break,
                _ => {}
            }
        }
    })
    .await;
}

async fn turn(
    thread: &CodexThread,
    spec: &TurnSpec,
    usage: &mut Option<(i64, i64, i64)>,
) -> anyhow::Result<String> {
    // Codex sends TurnComplete before it marks the thread idle, so a follow-up
    // turn submitted straight after one can be declined as NotIdle. That
    // window is short: retry for a bounded time, and only for that reason.
    let until = std::time::Instant::now() + NOT_IDLE_PATIENCE;
    loop {
        let mut request = TurnInputRequest::user_input(vec![UserInput::Text {
            text: spec.prompt.clone(),
            text_elements: Vec::new(),
        }]);
        request.start.final_output_json_schema = spec.output_schema.clone();
        match thread
            .start_turn_if_idle(request)
            .await
            .context("submit user input")?
        {
            StartIfIdleSubmission::Started { .. } => break,
            StartIfIdleSubmission::NotSubmitted {
                reason: NotSubmittedReason::NotIdle,
            } if std::time::Instant::now() < until => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            StartIfIdleSubmission::NotSubmitted { reason } => {
                bail!("turn input was not submitted: {reason:?}")
            }
        }
    }

    loop {
        let event = thread.next_event().await.context("read Codex event")?;
        match event.msg {
            EventMsg::TokenCount(count) => {
                if let Some(info) = count.info {
                    let t = info.total_token_usage;
                    *usage = Some((t.input_tokens, t.output_tokens, t.total_tokens));
                }
            }
            EventMsg::TurnComplete(done) => {
                return done
                    .last_agent_message
                    .context("the turn completed without a message");
            }
            EventMsg::Error(event) => bail!(event.message),
            EventMsg::TurnAborted(_) => bail!("turn aborted"),
            EventMsg::ExecApprovalRequest(_) => bail!("turn requested exec approval"),
            EventMsg::ApplyPatchApprovalRequest(_) => bail!("turn requested patch approval"),
            EventMsg::RequestPermissions(_) => bail!("turn requested permissions"),
            EventMsg::RequestUserInput(_) => bail!("turn requested user input"),
            EventMsg::DynamicToolCallRequest(_) => bail!("turn requested a dynamic tool call"),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeModel, Reply};

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("atomic-codex-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).expect("mkdir");
        d
    }

    /// Codex runs on threads with large stacks, as its entry point sets up.
    fn on_codex_runtime<F: std::future::Future<Output = ()> + Send + 'static>(f: F) {
        const STACK: usize = 16 * 1024 * 1024;
        std::thread::Builder::new()
            .stack_size(STACK)
            .spawn(move || {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_stack_size(STACK)
                    .enable_all()
                    .build()
                    .expect("runtime")
                    .block_on(f)
            })
            .expect("thread")
            .join()
            .expect("the test thread panicked");
    }

    #[test]
    fn a_scripted_model_drives_a_real_codex_thread_with_an_exact_call_count() {
        on_codex_runtime(async {
            let model = FakeModel::start(vec![Reply::Text("pong".into())])
                .await
                .expect("fake model");
            let (home, cwd) = (scratch("home"), scratch("cwd"));
            let endpoint = ModelEndpoint {
                base_url: model.base_url(),
                model: "synthetic".into(),
                api_key_env: None,
            };
            let answer = one_turn(
                &CodexPaths::without_helpers(),
                &home,
                &cwd,
                &endpoint,
                "ping",
            )
            .await
            .expect("one turn");
            assert_eq!(answer, "pong");
            assert_eq!(model.calls(), 1, "requests: {:#?}", model.requests());
            // The prompt reached the model.
            assert!(model.requests()[0].to_string().contains("ping"));
        });
    }

    /// Codex sends TurnComplete before it marks the thread idle; a follow-up
    /// submitted in that window must wait, not fail as NotIdle.
    #[test]
    fn follow_up_turns_submitted_back_to_back_all_run() {
        const TURNS: usize = 100;
        on_codex_runtime(async {
            let replies = (0..TURNS).map(|i| Reply::Text(format!("r{i}"))).collect();
            let model = FakeModel::start(replies).await.expect("fake model");
            let (home, cwd) = (scratch("home3"), scratch("cwd3"));
            let endpoint = ModelEndpoint {
                base_url: model.base_url(),
                model: "synthetic".into(),
                api_key_env: None,
            };
            let first = TurnSpec {
                prompt: "t0".into(),
                ..TurnSpec::default()
            };
            let mut seen = Vec::new();
            let (last, report) = converse(
                &CodexPaths::without_helpers(),
                &home,
                &cwd,
                &endpoint,
                &first,
                |answer| {
                    seen.push(answer.to_owned());
                    (seen.len() < TURNS).then(|| TurnSpec {
                        prompt: format!("t{}", seen.len()),
                        ..TurnSpec::default()
                    })
                },
            )
            .await
            .expect("every follow-up turn runs");
            assert_eq!(last, format!("r{}", TURNS - 1));
            assert_eq!(report.turns as usize, TURNS);
            assert_eq!(model.calls(), TURNS);
        });
    }

    #[test]
    fn sandboxes_map_to_codex_permission_profiles() {
        use crate::typed::Sandbox;
        assert_eq!(
            permission_profile(Sandbox::ReadOnly),
            PermissionProfile::read_only()
        );
        assert_eq!(
            permission_profile(Sandbox::WorkspaceWrite),
            PermissionProfile::workspace_write()
        );
        assert_ne!(
            permission_profile(Sandbox::ReadOnly),
            permission_profile(Sandbox::WorkspaceWrite)
        );
    }

    #[test]
    fn a_tool_call_costs_one_more_model_call() {
        on_codex_runtime(async {
            let model = FakeModel::start(vec![
                Reply::Tool {
                    name: "no_such_tool".into(),
                    arguments: serde_json::json!({}),
                },
                Reply::Text("done".into()),
            ])
            .await
            .expect("fake model");
            let (home, cwd) = (scratch("home2"), scratch("cwd2"));
            let endpoint = ModelEndpoint {
                base_url: model.base_url(),
                model: "synthetic".into(),
                api_key_env: None,
            };
            let answer = one_turn(&CodexPaths::without_helpers(), &home, &cwd, &endpoint, "go")
                .await
                .expect("one turn");
            assert_eq!(answer, "done");
            assert_eq!(model.calls(), 2);
            // The second call carries the failed tool call's result.
            assert!(model.requests()[1].to_string().contains("call_synthetic"));
        });
    }

    // -----------------------------------------------------------------------
    // Metering: each turn admitted first and settled once, each reported
    // token charged once.

    use crate::usage::{Limit, Meter, MeterFuture, Ticket};
    use std::sync::Mutex;

    /// Admits every turn with `limit` and records each settlement.
    #[derive(Default)]
    struct Recorder {
        limit: Limit,
        settled: Mutex<Vec<Outcome>>,
    }

    impl Meter for Recorder {
        fn admit(&self, call: Call) -> MeterFuture<'_, Ticket> {
            Box::pin(async move {
                Ok(Ticket {
                    request: format!("r:{}", call.index),
                    attempt: 1,
                    model: call.model,
                    admitted: std::time::Instant::now(),
                    limit: self.limit,
                })
            })
        }

        fn settle(&self, _: Ticket, outcome: Outcome) -> MeterFuture<'_, ()> {
            Box::pin(async move {
                self.settled.lock().expect("settled").push(outcome);
                Ok(())
            })
        }
    }

    async fn metered_conversation(
        tag: &str,
        replies: Vec<Reply>,
        turns: usize,
        limit: Limit,
    ) -> (anyhow::Result<(String, ThreadReport)>, Vec<Outcome>, usize) {
        let model = FakeModel::start(replies).await.expect("fake model");
        let (home, cwd) = (
            scratch(&format!("home-{tag}")),
            scratch(&format!("cwd-{tag}")),
        );
        let endpoint = ModelEndpoint {
            base_url: model.base_url(),
            model: "synthetic".into(),
            api_key_env: None,
        };
        let recorder = Arc::new(Recorder {
            limit,
            ..Recorder::default()
        });
        let meter = Metering::new(recorder.clone());
        let mut asked = 1;
        let ended = converse_async(
            &CodexPaths::without_helpers(),
            &home,
            &cwd,
            &endpoint,
            &meter,
            &TurnSpec {
                prompt: "t0".into(),
                ..TurnSpec::default()
            },
            |_| {
                let more = asked < turns;
                asked += 1;
                std::future::ready(more.then(|| TurnSpec {
                    prompt: format!("t{asked}"),
                    ..TurnSpec::default()
                }))
            },
        )
        .await;
        let settled = recorder.settled.lock().expect("settled").clone();
        (ended, settled, model.calls())
    }

    const ONE: Outcome = Outcome::Reported(Usage {
        input_tokens: 1,
        output_tokens: 1,
    });

    #[test]
    fn each_turn_settles_with_what_the_thread_grew_by_and_a_silent_turn_as_unknown() {
        on_codex_runtime(async {
            let (ended, settled, calls) = metered_conversation(
                "meter",
                vec![
                    Reply::Text("a".into()),
                    Reply::WithoutUsage("b".into()),
                    Reply::Text("c".into()),
                ],
                3,
                Limit::default(),
            )
            .await;
            let (_, report) = ended.expect("three turns");
            assert_eq!(calls, 3);
            // The thread's totals grow by one and one per reported turn; the
            // turn that reported nothing is unknown, never free, and the
            // next turn is charged only its own growth.
            assert_eq!(settled, [ONE, Outcome::Unknown, ONE]);
            assert_eq!((report.input_tokens, report.output_tokens), (2, 2));
        });
    }

    #[test]
    fn a_failed_turn_settles_with_the_usage_seen_before_it_failed() {
        on_codex_runtime(async {
            // The turn's first request reports its usage and calls a tool;
            // the second is refused, and the turn fails.
            let (ended, settled, calls) = metered_conversation(
                "failed",
                vec![
                    Reply::Tool {
                        name: "no_such_tool".into(),
                        arguments: serde_json::json!({}),
                    },
                    Reply::Http {
                        status: 400,
                        body:
                            r#"{"error": {"message": "refused", "type": "invalid_request_error"}}"#
                                .into(),
                    },
                ],
                1,
                Limit::default(),
            )
            .await;
            assert!(ended.is_err(), "the turn failed");
            assert!(calls >= 2, "{calls}");
            assert_eq!(settled, [ONE], "the failed turn kept the usage it reported");
        });
    }

    #[test]
    fn a_turn_is_cut_off_when_its_reserved_time_runs_out() {
        on_codex_runtime(async {
            let began = std::time::Instant::now();
            let (ended, settled, _) = metered_conversation(
                "slow",
                vec![Reply::Delayed {
                    after: std::time::Duration::from_secs(30),
                    reply: Box::new(Reply::Text("late".into())),
                }],
                1,
                Limit {
                    output_tokens: None,
                    time: Some(std::time::Duration::from_millis(500)),
                },
            )
            .await;
            let e = ended.expect_err("cut off");
            assert!(format!("{e:#}").contains("cut off"), "{e:#}");
            assert!(began.elapsed() < std::time::Duration::from_secs(20));
            assert_eq!(settled, [Outcome::Unknown]);
        });
    }
}
