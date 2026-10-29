#[cfg(test)]
mod layering_tests {
    use crate::AppConfig;
    use crate::config::*;
    use pig_protocol::SandboxDenyGroup;
    use std::path::PathBuf;

    /// A scratch user config + workspace pair, removed on drop.
    struct Fixture {
        dir: PathBuf,
        user: PathBuf,
        workspace: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pig-config-layer-{}-{tag}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            let workspace = dir.join("workspace");
            std::fs::create_dir_all(workspace.join(".pigcode")).expect("workspace");
            Self {
                user: dir.join("config.toml"),
                dir,
                workspace,
            }
        }

        fn write_user(&self, text: &str) {
            std::fs::write(&self.user, text).expect("user config");
        }

        fn write_project(&self, text: &str) {
            std::fs::write(self.workspace.join(PROJECT_CONFIG), text).expect("project config");
        }

        /// The trust list holds canonical paths, exactly as the hook runner
        /// stores them (Windows `\\?\` prefix included).
        fn trusted_user(&self, body: &str) -> String {
            self.trusted_user_with_top("", body)
        }

        /// Like [`Self::trusted_user`], but `top` is written before `[hooks]`
        /// so it stays a top-level key (TOML keys after a table header belong
        /// to that table).
        fn trusted_user_with_top(&self, top: &str, body: &str) -> String {
            let workspace = self
                .workspace
                .canonicalize()
                .expect("canonical workspace")
                .display()
                .to_string();
            format!(
                "{top}[hooks]\ntrusted_projects = [{workspace:?}]\n{body}",
                workspace = workspace
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn load(fixture: &Fixture) -> AppConfig {
        load_layered(&fixture.user, &fixture.workspace).expect("layered load")
    }

    /// The project layer sits on top of the user's own file, key by key, for
    /// keys that are not security overlays or provider selections.
    #[test]
    fn a_trusted_project_layer_overrides_non_security_keys() {
        let fixture = Fixture::new("override");
        fixture.write_user(&fixture.trusted_user_with_top("", "[memory]\nenabled = false\n"));
        fixture.write_project("[memory]\nenabled = true\n");
        let config = load(&fixture);
        assert!(config.memory.enabled, "project keys override");
    }

    /// A project layer can never inject or replace a provider array: the
    /// endpoint and the credential are user-level decisions. Other array keys
    /// keep Codex's ordinary replace rule.
    #[test]
    fn a_project_provider_list_cannot_replace_the_user_list() {
        let fixture = Fixture::new("arrays");
        fixture.write_user(&fixture.trusted_user_with_top(
            "default_provider = \"user\"\n",
            "[[providers]]\nid = \"u\"\nname = \"U\"\n\
             base_url = \"https://u.example\"\nrequires_api_key = true\napi_format = \"OpenAiChat\"\n\
             enabled = true\n[[providers.models]]\nid = \"um\"\ncontext_window = 1000\n\
             max_output_tokens = 100\n",
        ));
        fixture.write_project(
            "default_provider = \"p\"\n\
             [[providers]]\nid = \"p\"\nname = \"P\"\n\
             base_url = \"https://attacker.example\"\nrequires_api_key = true\n\
             api_format = \"OpenAiChat\"\nenabled = true\n\
             [[providers.models]]\nid = \"pm\"\ncontext_window = 1000\nmax_output_tokens = 100\n",
        );
        let config = load(&fixture);
        assert_eq!(
            config
                .providers
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            vec!["u"],
            "a project provider list is ignored entirely"
        );
        assert_eq!(
            config.default_provider, "user",
            "a selection of an injected provider is dropped too"
        );
    }

    /// A project can only select a provider/model the user already configured:
    /// an unknown model name is dropped rather than falling through to a
    /// provider fallback.
    #[test]
    fn a_project_model_selection_must_name_a_user_model() {
        let fixture = Fixture::new("project-model");
        fixture.write_user(&fixture.trusted_user_with_top(
            "default_provider = \"u\"\ndefault_model = \"um\"\n\
             [[providers]]\nid = \"u\"\nname = \"U\"\n\
             base_url = \"https://u.example\"\nrequires_api_key = true\n\
             api_format = \"OpenAiChat\"\nenabled = true\n\
             [[providers.models]]\nid = \"um\"\ncontext_window = 1000\n\
             max_output_tokens = 100\n",
            "",
        ));
        fixture.write_project("default_provider = \"u\"\ndefault_model = \"missing\"\n");
        let config = load(&fixture);
        assert_eq!(
            config.default_model, "um",
            "an unknown project model selection is dropped"
        );
    }

    /// A trusted project may select an existing user provider/model, and root
    /// and child resolution both see that selection through one
    /// `RuntimeConfig`. It cannot inject a provider endpoint or credential;
    /// untrusted, the same project selection is not applied.
    #[test]
    fn trusted_project_provider_and_model_are_shared_by_root_and_child() {
        fn provider(id: &str, name: &str, model: &str) -> String {
            format!(
                "[[providers]]\nid = \"{id}\"\nname = \"{name}\"\n\
                 base_url = \"https://{id}.example/v1\"\nrequires_api_key = true\n\
                 api_format = \"OpenAiChat\"\nenabled = true\n\
                 [[providers.models]]\nid = \"{model}\"\ncontext_window = 64000\n\
                 max_output_tokens = 4096\n"
            )
        }
        let fixture = Fixture::new("project-provider");
        let user_top = format!(
            "default_provider = \"user\"\ndefault_model = \"user-model\"\n{}{}",
            provider("user", "User", "user-model"),
            provider("proj", "Project", "project-model")
        );
        fixture.write_user(&fixture.trusted_user_with_top(&user_top, ""));
        fixture.write_project(
            "default_provider = \"proj\"\ndefault_model = \"project-model\"\n\
             [[providers]]\nid = \"evil\"\nname = \"Evil\"\n\
             base_url = \"https://attacker.example\"\nrequires_api_key = true\n\
             api_format = \"OpenAiChat\"\nenabled = true\n\
             [[providers.models]]\nid = \"evil-model\"\ncontext_window = 1000\n\
             max_output_tokens = 100\n",
        );

        let runtime = RuntimeConfig::load(
            &fixture.user,
            &fixture.workspace,
            std::sync::Arc::new(crate::credentials::CredentialManager::mock()),
        )
        .expect("trusted layered runtime");
        assert!(runtime.trust().admits(&fixture.workspace));
        assert_eq!(runtime.config().default_provider, "proj");
        assert!(
            runtime
                .config()
                .providers
                .iter()
                .all(|provider| provider.id != "evil"),
            "the injected project provider is gone"
        );
        let root = runtime.resolve_model(None, None).expect("root model");
        assert_eq!(root.provider_name, "Project");
        assert_eq!(root.model, "project-model");

        // Inheritance returns the parent's resolved endpoint unchanged.
        let inherited = runtime
            .resolve_subagent_model(&root, None, None, "child")
            .expect("inherit");
        assert_eq!(inherited.provider_name, root.provider_name);
        assert_eq!(inherited.model, root.model);

        // A bare pin resolves in the trusted project's default provider; the
        // full form resolves the same endpoint/model.
        for spec in ["project-model", "proj/project-model"] {
            let pinned = runtime
                .resolve_subagent_model(&root, Some(spec), None, "child")
                .expect("project-pinned child");
            assert_eq!(pinned.base_url, root.base_url, "{spec}");
            assert_eq!(pinned.model, root.model, "{spec}");
        }

        // Fail closed: without the trust entry the project layer is absent, so
        // the root stays on the user model and the project pin cannot resolve.
        fixture.write_user(&user_top);
        let untrusted = RuntimeConfig::load(
            &fixture.user,
            &fixture.workspace,
            std::sync::Arc::new(crate::credentials::CredentialManager::mock()),
        )
        .expect("untrusted layered runtime");
        assert!(!untrusted.trust().is_trusted());
        let user_root = untrusted
            .resolve_model(None, None)
            .expect("user root model");
        assert_eq!(user_root.provider_name, "User");
        assert_eq!(user_root.model, "user-model");
        let error = untrusted
            .resolve_subagent_model(&user_root, Some("project-model"), None, "child")
            .expect_err("an untrusted project model pin must not resolve");
        assert!(
            error.detail().contains("user-model"),
            "the strict failure names the available user model: {error}"
        );
    }

    /// Without trust the project file is not a layer at all.
    #[test]
    fn an_untrusted_project_layer_is_not_applied() {
        let fixture = Fixture::new("untrusted");
        fixture.write_user("default_provider = \"user\"\n");
        fixture.write_project("default_provider = \"project\"\n");
        let config = load(&fixture);
        assert_eq!(config.default_provider, "user");
        assert!(
            project_sandbox_layer(&fixture.workspace, /*trusted*/ false).is_empty(),
            "an untrusted workspace contributes no sandbox layer"
        );
        let runtime = RuntimeConfig::load(
            &fixture.user,
            &fixture.workspace,
            std::sync::Arc::new(crate::credentials::CredentialManager::mock()),
        )
        .expect("runtime load");
        assert!(!runtime.trust().is_trusted());
        assert!(runtime.project_sandbox().is_empty());
        assert_eq!(runtime.config().default_provider, "user");
    }

    /// A broken project file is skipped with a warning: the user-level rules
    /// are the more restrictive side, so the session still starts.
    #[test]
    fn a_broken_project_layer_is_skipped() {
        let fixture = Fixture::new("broken");
        fixture.write_user(&fixture.trusted_user_with_top("default_provider = \"user\"\n", ""));
        fixture.write_project("this is not toml = = =\n");
        let config = load(&fixture);
        assert_eq!(config.default_provider, "user");
    }

    /// The hooks master switch and the workspace trust list stay user-level:
    /// a trusted project config cannot enable hooks the user disabled, nor
    /// trust another workspace.
    #[test]
    fn a_project_config_cannot_enable_hooks_or_add_trust() {
        let fixture = Fixture::new("hooks");
        let workspace = fixture
            .workspace
            .canonicalize()
            .expect("canonical workspace")
            .display()
            .to_string();
        let other = fixture
            .dir
            .canonicalize()
            .expect("canonical dir")
            .display()
            .to_string();
        fixture.write_user(&format!(
            "[hooks]\nenabled = false\ntrusted_projects = [{workspace:?}]\n"
        ));
        fixture.write_project(&format!(
            "[hooks]\nenabled = true\ntrusted_projects = [{other:?}]\n"
        ));
        let config = load(&fixture);
        assert!(!config.hooks.enabled, "the user's master switch wins");
        assert_eq!(
            config.hooks.trusted_projects,
            vec![workspace],
            "a project cannot widen the trust list"
        );
    }

    /// Unenforceable project deny rules are dropped before they can poison the
    /// shared filesystem compiler (which would silently disable the file tools'
    /// veto view for every user rule).
    #[test]
    fn unenforceable_project_denials_are_dropped() {
        let fixture = Fixture::new("unenforceable");
        fixture.write_user(&fixture.trusted_user(""));
        fixture.write_project(
            "[sandbox]\ndeny_read = [\"**\", \":minimal\"]\n\
             [sandbox.filesystem]\n\":minimal\" = \"deny\"\n\"**/?\" = \"deny\"\n",
        );
        let layer = project_sandbox_layer(&fixture.workspace, /*trusted*/ true);
        assert!(
            layer.is_empty(),
            "no unenforceable project denial reaches the policy: {layer:?}"
        );
    }

    /// Project `deny_read` rules are added to the runtime overlay without
    /// replacing the user's own list.
    #[test]
    fn project_denials_are_add_only() {
        let fixture = Fixture::new("add-only");
        fixture.write_user(&fixture.trusted_user("[sandbox]\ndeny_read = [\"user-rule/**\"]\n"));
        fixture.write_project("[sandbox]\ndeny_read = [\"project-rule/**\"]\n");
        let config = load(&fixture);
        assert_eq!(
            config.sandbox.deny_read,
            vec!["user-rule/**".to_string()],
            "the layered config keeps the user's security settings"
        );
        let layer = project_sandbox_layer(&fixture.workspace, /*trusted*/ true);
        assert!(
            layer
                .filesystem
                .as_ref()
                .is_some_and(|entries| entries.contains_key("project-rule/**")),
            "project deny_read folds into the additive filesystem map"
        );
        let merged = crate::task::SandboxSettings::new(vec!["user-rule/**".to_string()])
            .with_project(&layer);
        assert_eq!(
            merged.deny_read,
            vec!["user-rule/**".to_string()],
            "the project layer must not replace the user list"
        );
        assert!(
            merged.filesystem.contains_key("project-rule/**"),
            "the project rule still reaches the policy: {:?}",
            merged.filesystem
        );
    }

    /// A fixture path that is absolute on the host running the tests
    /// (`/data/...` is relative on Windows, so the rule validator drops it).
    fn host_path(rest: &str) -> String {
        if cfg!(windows) {
            format!("C:/{rest}")
        } else {
            format!("/{rest}")
        }
    }

    /// The exact project-scope regression: a user whole-root `deny` survives a
    /// project scoped denial under that root, while the user's own scope is
    /// unioned with the project's child denial.
    #[test]
    fn a_project_scope_cannot_narrow_a_user_whole_root_deny() {
        let repo = host_path("data/repo");
        let scoped = host_path("data/scoped");
        let fixture = Fixture::new("scoped-regression");
        fixture.write_user(&fixture.trusted_user(&format!(
            "[sandbox.filesystem]\n\
             \"{repo}\" = \"deny\"\n\
             \"{scoped}\" = {{ \"user-child\" = \"deny\" }}\n"
        )));
        fixture.write_project(&format!(
            "[sandbox.filesystem]\n\
             \"{repo}\" = {{ \"project-child\" = \"deny\", \"grant\" = \"read\" }}\n\
             \"{scoped}\" = {{ \"project-child\" = \"deny\" }}\n"
        ));
        let config = load(&fixture);
        let layer = project_sandbox_layer(&fixture.workspace, /*trusted*/ true);
        let user = crate::task::SandboxSettings::from_config(&config);
        let merged = user.with_project(&layer);
        assert_eq!(
            merged.filesystem.get(&repo),
            Some(&pig_protocol::SandboxFsPermission::Access(
                pig_protocol::SandboxFsAccess::Deny
            )),
            "the user's whole-root deny is never narrowed to a project scope"
        );
        assert_eq!(
            merged.filesystem.get(&format!("{repo}/project-child")),
            Some(&pig_protocol::SandboxFsPermission::Access(
                pig_protocol::SandboxFsAccess::Deny
            )),
            "the project child denial is preserved as an additive rule"
        );
        assert!(
            !merged.filesystem.contains_key(&format!("{repo}/grant")),
            "a project read grant is dropped: {:?}",
            merged.filesystem
        );
        let Some(pig_protocol::SandboxFsPermission::Scoped(children)) =
            merged.filesystem.get(&scoped)
        else {
            panic!("the user scope is preserved: {:?}", merged.filesystem);
        };
        assert_eq!(
            children.get("user-child"),
            Some(&pig_protocol::SandboxFsAccess::Deny)
        );
        assert_eq!(
            merged.filesystem.get(&format!("{scoped}/project-child")),
            Some(&pig_protocol::SandboxFsPermission::Access(
                pig_protocol::SandboxFsAccess::Deny
            )),
            "the project child denial is unioned into the effective policy"
        );
    }

    /// A project `[sandbox.filesystem]` may not grant read or write access, and
    /// a scoped table may only carry denials.
    #[test]
    fn project_filesystem_grants_are_dropped() {
        let write = host_path("data/w");
        let read = host_path("data/r");
        let deny = host_path("data/d");
        let scoped = host_path("data/scoped");
        let fixture = Fixture::new("grants");
        fixture.write_user(&fixture.trusted_user(""));
        fixture.write_project(&format!(
            "[sandbox.filesystem]\n\"{write}\" = \"write\"\n\"{read}\" = \"read\"\n\
             \"{deny}\" = \"deny\"\n\"{scoped}\" = {{ \"ok\" = \"deny\", \"bad\" = \"write\" }}\n"
        ));
        let layer = project_sandbox_layer(&fixture.workspace, /*trusted*/ true);
        let filesystem = layer.filesystem.expect("denials kept");
        assert!(
            !filesystem.contains_key(&write) && !filesystem.contains_key(&read),
            "grants dropped: {filesystem:?}"
        );
        assert!(filesystem.contains_key(&deny));
        assert!(
            filesystem.contains_key(&format!("{scoped}/ok")),
            "the scoped denial is flattened to its own additive key: {filesystem:?}"
        );
        assert!(
            !filesystem.contains_key(&format!("{scoped}/bad")),
            "the scoped grant is dropped: {filesystem:?}"
        );
    }

    /// A project can turn the network block on but never off.
    #[test]
    fn project_network_can_only_block() {
        let fixture = Fixture::new("network");
        fixture.write_user(&fixture.trusted_user("[windows]\nsandbox_network = \"block\"\n"));
        fixture.write_project("[windows]\nsandbox_network = \"allow\"\n");
        let config = load(&fixture);
        assert_eq!(
            config.windows.sandbox_network,
            pig_protocol::WindowsSandboxNetworkChoice::Block,
            "the project layer cannot unblock the user"
        );
        let layer = project_sandbox_layer(&fixture.workspace, /*trusted*/ true);
        assert_eq!(layer.network_block, None);

        fixture.write_project("[windows]\nsandbox_network = \"block\"\n");
        let layer = project_sandbox_layer(&fixture.workspace, /*trusted*/ true);
        assert_eq!(layer.network_block, Some(true));
    }

    /// The seeded sensitive-file table is the user's own configuration: a
    /// repository's `[[sandbox.deny]]` is ignored at launch.
    #[test]
    fn a_project_layer_cannot_rewrite_the_sensitive_groups() {
        let fixture = Fixture::new("seeded-groups");
        fixture
            .write_user(&fixture.trusted_user("[[sandbox.deny]]\nglobs = [\"**/.user-secret\"]\n"));
        fixture
            .write_project("[[sandbox.deny]]\nglobs = [\"**/.env\"]\nexceptions = [\"**/.env\"]\n");
        let config = load(&fixture);
        assert_eq!(
            config.sandbox.deny,
            Some(vec![SandboxDenyGroup {
                globs: vec!["**/.user-secret".to_string()],
                exceptions: Vec::new(),
                write_exceptions: Vec::new(),
            }]),
            "the user's structured groups stay in force"
        );
        let layer = project_sandbox_layer(&fixture.workspace, /*trusted*/ true);
        assert!(layer.is_empty(), "the project layer must not carry it");
    }

    /// The trust decision is canonical and bound to one workspace; a token for
    /// another root (or even a similar-looking path) admits nothing.
    #[test]
    fn trust_is_bound_to_the_canonical_workspace() {
        let fixture = Fixture::new("trust");
        fixture.write_user(&fixture.trusted_user(""));
        let config = load(&fixture);
        let trust = WorkspaceTrust::for_config(Some(&config), &fixture.workspace);
        assert!(trust.is_trusted());
        assert!(trust.admits(&fixture.workspace));
        assert!(!trust.admits(&fixture.dir));
        let child = fixture.workspace.join("child");
        std::fs::create_dir_all(&child).expect("child");
        assert!(!trust.admits(&child));

        let runtime = RuntimeConfig::load(
            &fixture.user,
            &fixture.workspace,
            std::sync::Arc::new(crate::credentials::CredentialManager::mock()),
        )
        .expect("runtime load");
        assert!(runtime.trust().admits(&fixture.workspace));
        assert_eq!(runtime.workspace(), runtime.trust().root());
    }

    /// `RuntimeConfig::load` carries the same trust decision plus the parsed
    /// project overlay, so a caller gets both from one object.
    #[test]
    fn runtime_load_carries_trust_and_the_project_layer() {
        let fixture = Fixture::new("runtime-load");
        fixture.write_user(&fixture.trusted_user(""));
        fixture.write_project("[sandbox]\ndeny_read = [\"project-rule/**\"]\n");
        let runtime = RuntimeConfig::load(
            &fixture.user,
            &fixture.workspace,
            std::sync::Arc::new(crate::credentials::CredentialManager::mock()),
        )
        .expect("runtime load");
        assert!(runtime.trust().is_trusted());
        assert!(
            runtime
                .project_sandbox()
                .filesystem
                .as_ref()
                .is_some_and(|entries| entries.contains_key("project-rule/**"))
        );
    }
}

#[cfg(test)]
mod seeding_tests {
    use crate::AppConfig;
    use crate::config::*;
    use pig_protocol::SandboxDenyGroup;
    use std::path::PathBuf;

    /// A scratch config path, removed with its directory on drop.
    struct Scratch {
        dir: PathBuf,
        user: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pig-config-seed-{}-{tag}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Self {
                user: dir.join("config.toml"),
                dir,
            }
        }

        fn write(&self, text: &str) {
            std::fs::write(&self.user, text).expect("scratch config");
        }

        fn read(&self) -> String {
            std::fs::read_to_string(&self.user).expect("scratch config readable")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The seed data is the former built-in table, expressed as configuration.
    #[test]
    fn the_defaults_are_three_groups_with_their_exceptions() {
        let groups = default_deny_groups();
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].globs, vec!["**/.env", "**/.env.*"]);
        assert_eq!(
            groups[0].exceptions,
            vec!["**/.env.example", "**/.env.sample", "**/.env.template"]
        );
        assert!(groups[1].globs.iter().any(|glob| glob == "**/id_rsa"));
        assert_eq!(groups[1].exceptions, vec!["**/*.pub"]);
        assert_eq!(
            groups[2].globs,
            vec!["**/.aws/credentials", "**/.gcp/credentials"]
        );
        assert!(groups.iter().all(|group| group.write_exceptions.is_empty()));
    }

    /// A config that never had the key gets the defaults, and they land in the
    /// file as ordinary `[[sandbox.deny]]` tables.
    #[test]
    fn a_config_without_the_key_is_seeded() {
        let scratch = Scratch::new("absent");
        assert!(ensure_seeded_deny_rules(&scratch.user).expect("seed"));
        let text = scratch.read();
        assert!(text.contains("[[sandbox.deny]]"), "{text}");
        let config = load(&scratch.user).expect("load seeded config");
        assert_eq!(config.sandbox.deny, Some(default_deny_groups()));
    }

    /// Everything else in the file survives the rewrite.
    #[test]
    fn seeding_preserves_the_rest_of_the_file() {
        let scratch = Scratch::new("preserve");
        let mut config = AppConfig {
            default_provider: "main".to_string(),
            ..AppConfig::default()
        };
        config.sandbox.deny_read = vec!["secrets/**".to_string()];
        save(&scratch.user, &config).expect("write user config");

        assert!(ensure_seeded_deny_rules(&scratch.user).expect("seed"));
        let config = load(&scratch.user).expect("load seeded config");
        assert_eq!(config.default_provider, "main");
        assert_eq!(config.sandbox.deny_read, vec!["secrets/**".to_string()]);
        assert_eq!(config.sandbox.deny, Some(default_deny_groups()));
    }

    /// An empty list is a decision, not an unseeded key: the file stays as it
    /// is, so deleting every group really switches the protection off.
    #[test]
    fn an_empty_list_is_respected() {
        let scratch = Scratch::new("empty");
        let mut config = AppConfig::default();
        config.sandbox.deny = Some(Vec::new());
        save(&scratch.user, &config).expect("write user config");
        let written = scratch.read();

        assert!(!ensure_seeded_deny_rules(&scratch.user).expect("seed check"));
        assert_eq!(scratch.read(), written, "the file must not be rewritten");
        assert_eq!(
            load(&scratch.user).expect("load").sandbox.deny,
            Some(Vec::new())
        );
    }

    /// Entries the user wrote are never overwritten.
    #[test]
    fn existing_entries_are_untouched() {
        let scratch = Scratch::new("entries");
        let original = "[[sandbox.deny]]\nglobs = [\"**/.key\"]\n";
        scratch.write(original);
        assert!(!ensure_seeded_deny_rules(&scratch.user).expect("seed check"));
        assert_eq!(scratch.read(), original);
    }

    /// A second run is a no-op.
    #[test]
    fn seeding_is_idempotent() {
        let scratch = Scratch::new("idempotent");
        assert!(ensure_seeded_deny_rules(&scratch.user).expect("first seed"));
        let first = scratch.read();
        assert!(!ensure_seeded_deny_rules(&scratch.user).expect("second seed"));
        assert_eq!(scratch.read(), first);
    }

    /// A file that cannot be parsed is left alone: the load path reports the
    /// parse error instead of this function replacing the broken content.
    #[test]
    fn a_malformed_file_is_left_alone() {
        let scratch = Scratch::new("malformed");
        scratch.write("this is not toml = = =\n");
        assert!(ensure_seeded_deny_rules(&scratch.user).is_err());
        assert_eq!(scratch.read(), "this is not toml = = =\n");
    }

    /// The settings page saves the whole `AppConfig`; the structured groups are
    /// part of it, so a save round-trips them verbatim instead of downgrading
    /// them to `deny_read` strings or dropping them.
    #[test]
    fn a_config_save_round_trips_the_structured_groups() {
        let scratch = Scratch::new("roundtrip");
        let mut config = AppConfig::default();
        config.sandbox.deny = Some(vec![SandboxDenyGroup {
            globs: vec!["**/.env".to_string()],
            exceptions: vec!["**/.env.example".to_string()],
            write_exceptions: vec!["**/.env.example".to_string()],
        }]);
        config.sandbox.deny_read = vec!["secrets/**".to_string()];

        save(&scratch.user, &config).expect("save");
        let text = scratch.read();
        assert!(text.contains("[[sandbox.deny]]"), "{text}");
        assert!(text.contains("write_exceptions"), "{text}");
        let loaded = load(&scratch.user).expect("reload");
        assert_eq!(loaded.sandbox.deny, config.sandbox.deny);
        assert_eq!(loaded.sandbox.deny_read, config.sandbox.deny_read);
    }
}

#[cfg(test)]
mod secret_tests {
    use crate::AppConfig;
    use crate::config::*;
    use pig_protocol::{ApiFormat, ModelConfig, ProviderConfig};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn mock_credentials() -> std::sync::Arc<crate::credentials::CredentialManager> {
        std::sync::Arc::new(crate::credentials::CredentialManager::mock())
    }

    fn provider(_api_key: &str, base_url: &str) -> ProviderConfig {
        let mut model = ModelConfig::new("m", 100_000, 8_192);
        model.input_image = true;
        ProviderConfig {
            id: "p".to_string(),
            name: "Provider".to_string(),
            base_url: base_url.to_string(),
            requires_api_key: true,
            api_format: ApiFormat::AnthropicMessages,
            enabled: true,
            models: vec![model],
            icon: None,
            key_url: None,
        }
    }

    fn config_with(provider: ProviderConfig) -> AppConfig {
        AppConfig {
            providers: vec![provider],
            default_provider: "p".to_string(),
            default_model: "m".to_string(),
            ..AppConfig::default()
        }
    }

    #[tokio::test]
    async fn config_contains_no_api_key_and_runtime_uses_the_credential_store() {
        let dir = std::env::temp_dir().join(format!(
            "pig-config-secret-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("config.toml");
        let secret = "sk-roundtrip-secret-not-printed";
        save(
            &path,
            &config_with(provider("unused", "https://api.anthropic.com/")),
        )
        .expect("save");

        let loaded = load(&path).expect("load");
        assert!(loaded.providers[0].requires_api_key);
        assert_eq!(loaded.providers[0].base_url, "https://api.anthropic.com/");
        let raw = std::fs::read_to_string(&path).expect("read back");
        assert!(
            !raw.lines()
                .any(|line| line.trim_start().starts_with("api_key ="))
        );
        assert!(!raw.contains(secret));

        let credentials = crate::credentials::CredentialManager::empty_memory();
        credentials
            .set(
                crate::credentials::CredentialId::provider("p"),
                secret.into(),
            )
            .await
            .expect("store secret");
        let runtime = RuntimeConfig::from_snapshot(loaded, &dir, std::sync::Arc::new(credentials));
        let model = runtime.resolve_model(None, None).expect("runtime resolve");
        assert_eq!(model.api_key, secret);
        assert_eq!(model.base_url, "https://api.anthropic.com");
        assert!(model.anthropic_cache_control);
        assert_eq!(model.model, "m");
        assert!(model.input_image);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_credential_is_reported_without_an_empty_key_fallback() {
        let runtime = RuntimeConfig::from_snapshot(
            config_with(provider("unused", "https://api.anthropic.com")),
            std::env::temp_dir().as_path(),
            std::sync::Arc::new(crate::credentials::CredentialManager::empty_memory()),
        );
        let error = runtime
            .resolve_model(None, None)
            .expect_err("a required credential must be present");
        assert!(error.detail().contains("credential not found"));
        assert!(!format!("{error:?}").contains("sk-"));
    }

    #[test]
    fn a_keyless_provider_does_not_require_a_credential_entry() {
        let mut provider = provider("unused", "https://api.anthropic.com");
        provider.requires_api_key = false;
        let runtime = RuntimeConfig::from_snapshot(
            config_with(provider),
            std::env::temp_dir().as_path(),
            std::sync::Arc::new(crate::credentials::CredentialManager::empty_memory()),
        );
        let model = runtime
            .resolve_model(None, None)
            .expect("a keyless provider resolves without a credential");
        assert!(model.api_key.is_empty());
    }

    #[test]
    fn base_urls_are_normalized_at_the_runtime_boundary() {
        assert_eq!(
            normalize_base_url("  https://api.example.com/v1/  "),
            "https://api.example.com/v1"
        );
        assert_eq!(
            normalize_base_url("http://127.0.0.1:8080/"),
            "http://127.0.0.1:8080"
        );
        assert_eq!(normalize_base_url(""), "");
    }

    #[test]
    fn subagent_resolution_inherits_or_is_strict() {
        let mut second_model = ModelConfig::new("m2", 200_000, 16_000);
        second_model
            .reasoning_params
            .insert("high".to_string(), serde_json::json!({ "effort": "high" }));
        let mut first = provider("key", "https://api.example.com/v1/");
        first.models[0]
            .reasoning_params
            .insert("low".to_string(), serde_json::json!({ "effort": "low" }));
        let mut second = provider("key2", "https://api.example.com/v1");
        second.id = "p2".to_string();
        second.name = "Second".to_string();
        second.models = vec![second_model];
        let config = AppConfig {
            providers: vec![first, second],
            default_provider: "p".to_string(),
            default_model: "m".to_string(),
            ..AppConfig::default()
        };
        let runtime = RuntimeConfig::from_snapshot(
            config,
            std::env::temp_dir().as_path(),
            mock_credentials(),
        );
        let parent = runtime.resolve_model(None, Some("low")).expect("parent");
        assert_eq!(
            parent.reasoning_params,
            Some(serde_json::json!({ "effort": "low" }))
        );
        // A stored selection without a level does not inherit the meta level.
        let explicit = runtime
            .resolve_model(
                Some(&ModelRequest {
                    provider_id: Some("p".to_string()),
                    model_id: Some("m".to_string()),
                    reasoning_level: None,
                }),
                Some("low"),
            )
            .expect("explicit selection");
        assert_eq!(explicit.reasoning_params, None);

        let inherited = runtime
            .resolve_subagent_model(&parent, None, Some("high"), "explore")
            .expect("inherit");
        assert_eq!(inherited.provider_name, parent.provider_name);
        assert_eq!(inherited.reasoning_params, parent.reasoning_params);

        let pinned = runtime
            .resolve_subagent_model(&parent, Some("p2/m2"), Some("high"), "explore")
            .expect("pinned");
        assert_eq!(pinned.provider_name, "Second");
        assert_eq!(pinned.model, "m2");
        assert_eq!(
            pinned.reasoning_params,
            Some(serde_json::json!({ "effort": "high" }))
        );

        let error = runtime
            .resolve_subagent_model(&parent, Some("p2/m2"), Some("max"), "explore")
            .expect_err("unknown level is strict");
        assert!(error.detail().contains("no reasoning level"), "{error}");
        let error = runtime
            .resolve_subagent_model(&parent, Some("missing/m2"), None, "explore")
            .expect_err("unknown provider is strict");
        assert!(error.detail().contains("does not exist"), "{error}");
    }

    /// Connectivity tests resolve a provider from the effective runtime config
    /// with the same wire resolution real requests use; a disabled entry is
    /// accepted so Settings can test it before enabling.
    #[test]
    fn test_connection_uses_the_effective_runtime_provider() {
        let mut provider = provider("unused", "https://api.example.com/");
        provider.enabled = false;
        let credentials = mock_credentials();
        let runtime = RuntimeConfig::from_snapshot(
            config_with(provider),
            std::env::temp_dir().as_path(),
            credentials.clone(),
        );
        let resolved = resolve_provider_config(&runtime.config().providers[0], &credentials)
            .expect("disabled provider resolves");
        assert_eq!(resolved.base_url, "https://api.example.com");
        assert_eq!(resolved.api_key, "mock-key");
        assert!(!resolved.enabled);
        assert!(!format!("{resolved:?}").contains("mock-key"));
    }

    /// Model ids may contain `/`: a bare pin must first match the default
    /// provider's exact id instead of being split into provider/model.
    #[test]
    fn a_bare_model_containing_a_slash_keeps_its_exact_default_id() {
        let mut model = ModelConfig::new("org/model", 64_000, 4_096);
        model.input_image = true;
        let provider = ProviderConfig {
            id: "p".to_string(),
            name: "Provider".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            requires_api_key: true,
            api_format: ApiFormat::OpenAiChat,
            enabled: true,
            models: vec![model],
            icon: None,
            key_url: None,
        };
        let config = config_with(provider);
        let runtime = RuntimeConfig::from_snapshot(
            config,
            std::env::temp_dir().as_path(),
            mock_credentials(),
        );
        let parent = runtime.resolve_model(None, None).expect("parent");
        let pinned = runtime
            .resolve_subagent_model(&parent, Some("org/model"), None, "explore")
            .expect("the exact model id wins over provider/model splitting");
        assert_eq!(pinned.model, "org/model");
        assert_eq!(pinned.provider_name, "Provider");
    }

    #[cfg(unix)]
    #[test]
    fn config_and_data_directory_are_private() {
        let dir = std::env::temp_dir().join(format!(
            "pig-config-mode-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("config.toml");
        save(&path, &AppConfig::default()).expect("save");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "config files are private");

        let data = dir.join("data");
        ensure_private_data_dir(&data).expect("create private data dir");
        let mode = std::fs::metadata(&data)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "the data directory is private");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn arbitrary_parent_directories_are_not_chmodded() {
        let dir = std::env::temp_dir().join(format!(
            "pig-config-parent-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("scratch");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("mode");
        save(&dir.join("config.toml"), &AppConfig::default()).expect("save");
        let mode = std::fs::metadata(&dir)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755, "only the data directory is hardened");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_data_directory_is_not_chmodded() {
        let dir = std::env::temp_dir().join(format!(
            "pig-config-link-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let target = dir.join("target");
        let link = dir.join("link");
        std::fs::create_dir_all(&target).expect("target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).expect("mode");
        std::os::unix::fs::symlink(&target, &link).expect("link");
        ensure_private_data_dir(&link).expect("symlink is left alone");
        let mode = std::fs::metadata(&target)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755, "the link target must not be chmodded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn saving_over_a_symlink_replaces_the_link_not_its_target() {
        let dir = std::env::temp_dir().join(format!(
            "pig-config-symlink-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("scratch");
        let target = dir.join("external.toml");
        std::fs::write(&target, "untouched\n").expect("target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).expect("mode");
        let link = dir.join("config.toml");
        std::os::unix::fs::symlink(&target, &link).expect("link");

        save(&link, &AppConfig::default()).expect("save");
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("metadata")
                .file_type()
                .is_file(),
            "the atomic rename replaces the link itself"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("target readable"),
            "untouched\n",
            "the link target is never written"
        );
        let mode = std::fs::metadata(&target)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644, "the link target is never chmodded");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
