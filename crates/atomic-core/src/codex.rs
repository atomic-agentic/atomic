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
use codex_core_api::Notice;
use codex_core_api::OPENAI_PROVIDER_ID;
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
use std::path::Path;
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
    let config = new_config(endpoint, codex_home, cwd, paths.0.clone()).await?;
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

    let answer = run_turn(&thread, prompt.to_owned()).await;
    let shutdown_result = thread.shutdown_and_wait().await;
    let _ = thread_manager.remove_thread(&thread_id).await;

    let answer = answer?;
    shutdown_result.context("shut down Codex thread")?;
    Ok(answer)
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

async fn run_turn(thread: &CodexThread, prompt: String) -> anyhow::Result<String> {
    let submission = thread
        .start_turn_if_idle(TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt,
            text_elements: Vec::new(),
        }]))
        .await
        .context("submit user input")?;
    if let StartIfIdleSubmission::NotSubmitted { reason } = submission {
        bail!("turn input was not submitted: {reason:?}");
    }

    loop {
        let event = thread.next_event().await.context("read Codex event")?;
        match event.msg {
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
}
