use serde_json::{json, Value};
use tether_policy::config::{Action, OnTimeout};
use tether_policy::{
    AllowAll, Limits, Policy, PolicyConfig, PolicyError, RulePolicy, RunCtx, Verdict,
};
use tether_types::{Decision, ToolCall};

const EXAMPLE: &str = include_str!("../../../tether.example.toml");

fn call(server: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: "call_1".into(),
        name: name.into(),
        server: server.into(),
        arguments,
        external: false,
    }
}

fn exec(argv: &[&str]) -> ToolCall {
    call("sandbox", "exec", json!({ "argv": argv }))
}

fn write(path: &str) -> ToolCall {
    call(
        "sandbox",
        "write_file",
        json!({"path": path, "content": ""}),
    )
}

fn decide(policy: &RulePolicy, call: &ToolCall) -> Verdict {
    policy.decide(call, &RunCtx::default())
}

/// The outcome and the rule name, which is what the trace records.
fn outcome(policy: &RulePolicy, call: &ToolCall) -> (&'static str, Option<String>) {
    let verdict = decide(policy, call);
    let outcome = match verdict.decision {
        Decision::Allow => "allow",
        Decision::Deny { .. } => "deny",
        Decision::Ask { .. } => "ask",
    };
    (outcome, verdict.rule)
}

fn rule(name: &str) -> Option<String> {
    Some(name.to_owned())
}

#[test]
fn example_file_parses_and_decides() {
    let policy = RulePolicy::from_toml(EXAMPLE).expect("example policy");
    let config = policy.config();
    assert_eq!(config.rules.len(), 3);
    assert_eq!(config.budget.max_tokens, Some(2_000_000));
    assert_eq!(config.budget.max_usd, Some(5.0));
    assert_eq!(config.budget.ask_timeout_secs, Some(600));

    assert_eq!(
        outcome(&policy, &write("src/main.rs")),
        ("allow", rule("write-src"))
    );
    assert_eq!(
        outcome(&policy, &write("src/deep/er/mod.rs")),
        ("allow", rule("write-src"))
    );
    assert_eq!(
        outcome(&policy, &exec(&["rm", "-rf", "."])),
        ("ask", rule("risky-commands"))
    );
    assert_eq!(
        outcome(&policy, &exec(&["git", "push"])),
        ("ask", rule("risky-commands"))
    );
    assert_eq!(
        outcome(
            &policy,
            &call("net", "fetch", json!({"url": "https://example.com"}))
        ),
        ("deny", rule("no-network"))
    );
}

#[test]
fn a_call_no_rule_matches_is_an_ask_with_no_rule() {
    let policy = RulePolicy::from_toml(EXAMPLE).expect("example policy");
    for unmatched in [
        write("README.md"),
        exec(&["ls"]),
        call("github", "create_issue", json!({})),
    ] {
        assert_eq!(outcome(&policy, &unmatched), ("ask", None), "{unmatched:?}");
    }
    let Decision::Ask { prompt } = decide(&policy, &exec(&["ls"])).decision else {
        panic!("not an ask")
    };
    assert_eq!(prompt, r#"Allow sandbox.exec {"argv":["ls"]}?"#);

    // No file at all is the same: everything asks.
    assert_eq!(
        outcome(&RulePolicy::default(), &exec(&["ls"])),
        ("ask", None)
    );
}

#[test]
fn deny_carries_its_reason_and_ask_its_prompt() {
    let policy = RulePolicy::from_toml(
        r#"
        [[rule]]
        match = { tool = "net.*" }
        action = "deny"
        reason = "no network in this run"

        [[rule]]
        match = { argv0 = "rm" }
        action = "deny"

        [[rule]]
        match = { argv0 = "git" }
        action = "ask"
        reason = "touches the remote"
        "#,
    )
    .expect("policy");

    let verdict = decide(&policy, &call("net", "fetch", json!({})));
    assert_eq!(
        verdict.decision,
        Decision::Deny {
            reason: "no network in this run".into()
        }
    );
    let verdict = decide(&policy, &exec(&["rm", "x"]));
    assert_eq!(
        verdict.decision,
        Decision::Deny {
            reason: "denied by rule rule-2".into()
        }
    );
    let verdict = decide(&policy, &exec(&["git", "push"]));
    assert_eq!(
        verdict.decision,
        Decision::Ask {
            prompt: r#"Allow sandbox.exec {"argv":["git","push"]}? (touches the remote)"#.into()
        }
    );
}

#[test]
fn first_match_wins() {
    let policy = RulePolicy::from_toml(
        r#"
        [[rule]]
        name = "tests-are-fine"
        match = { tool = "sandbox.exec", argv0 = "cargo" }
        action = "allow"

        [[rule]]
        name = "no-exec"
        match = { tool = "sandbox.exec" }
        action = "deny"

        [[rule]]
        name = "everything-else"
        action = "allow"
        "#,
    )
    .expect("policy");

    assert_eq!(
        outcome(&policy, &exec(&["cargo", "test"])),
        ("allow", rule("tests-are-fine"))
    );
    assert_eq!(
        outcome(&policy, &exec(&["rm", "-rf", "/"])),
        ("deny", rule("no-exec"))
    );
    // A rule with no `match` is a catch-all.
    assert_eq!(
        outcome(&policy, &write("anywhere.txt")),
        ("allow", rule("everything-else"))
    );
}

#[test]
fn argv0_matches_the_command_name_however_it_is_spelled() {
    let policy = RulePolicy::from_toml(
        r#"
        [[rule]]
        name = "no-rm"
        match = { argv0 = "rm" }
        action = "deny"
        "#,
    )
    .expect("policy");

    for argv in [
        &["rm", "-rf", "."][..],
        &["/bin/rm", "x"],
        &["./rm"],
        &["../../bin/rm"],
    ] {
        assert_eq!(
            outcome(&policy, &exec(argv)),
            ("deny", rule("no-rm")),
            "{argv:?}"
        );
    }
    for argv in [&["rmdir", "x"][..], &["ls", "rm"], &[]] {
        assert_eq!(outcome(&policy, &exec(argv)), ("ask", None), "{argv:?}");
    }
    // A call with no argv at all cannot match an argv0 rule.
    assert_eq!(outcome(&policy, &write("rm")), ("ask", None));
    assert_eq!(
        outcome(
            &policy,
            &call("sandbox", "exec", json!({"argv": "rm -rf ."}))
        ),
        ("ask", None)
    );
}

#[test]
fn path_globs_cannot_be_dodged_with_dot_segments() {
    let policy = RulePolicy::from_toml(
        r#"
        [[rule]]
        name = "write-src"
        match = { tool = "sandbox.write_file", path = ["src/**", "tests/**"] }
        action = "allow"
        "#,
    )
    .expect("policy");

    for inside in [
        "src/a.rs",
        "./src/a.rs",
        "src/./x/../a.rs",
        "tests/t.rs",
        "docs/../src/a.rs",
    ] {
        assert_eq!(
            outcome(&policy, &write(inside)),
            ("allow", rule("write-src")),
            "{inside}"
        );
    }
    for outside in [
        "src/../Cargo.toml",
        "src/../../etc/passwd",
        "../src/a.rs",
        "/src/a.rs",
        "/etc/passwd",
        "srcs/a.rs",
        "src",
    ] {
        assert_eq!(
            outcome(&policy, &write(outside)),
            ("ask", None),
            "{outside}"
        );
    }
    // Same path, different tool: the tool key must match too.
    assert_eq!(
        outcome(
            &policy,
            &call("sandbox", "read_file", json!({"path": "src/a.rs"}))
        ),
        ("ask", None)
    );
}

#[test]
fn tool_patterns_match_the_qualified_name() {
    let policy = RulePolicy::from_toml(
        r#"
        [[rule]]
        name = "github"
        match = { tool = "github.*" }
        action = "allow"

        [[rule]]
        name = "bare-exec"
        match = { tool = "exec" }
        action = "allow"
        "#,
    )
    .expect("policy");

    assert_eq!(
        outcome(&policy, &call("github", "create_issue", json!({}))),
        ("allow", rule("github"))
    );
    // A name the registry prefixed on a collision reads the same to a rule.
    assert_eq!(
        outcome(&policy, &call("github", "github__create_issue", json!({}))),
        ("allow", rule("github"))
    );
    // The server is part of the name, so a bare pattern does not match.
    assert_eq!(outcome(&policy, &exec(&["ls"])), ("ask", None));
}

#[test]
fn rule_options_are_parsed() {
    let config = PolicyConfig::from_toml(
        r#"
        [[rule]]
        match = { argv0 = ["rm", "git"] }
        action = "ask"
        on_timeout = "deny"

        [[rule]]
        name = "rest"
        action = "allow"
        "#,
    )
    .expect("config");

    assert_eq!(config.rules[0].name, "rule-1");
    assert_eq!(config.rules[0].action, Action::Ask);
    assert_eq!(config.rules[0].on_timeout, OnTimeout::Deny);
    assert_eq!(config.rules[0].matcher.argv0, ["rm", "git"]);
    assert_eq!(config.rules[1].on_timeout, OnTimeout::Halt);
    assert!(config.rules[1].matcher.tool.is_empty());
}

#[test]
fn mistakes_in_the_file_are_errors() {
    let invalid = |text: &str| match PolicyConfig::from_toml(text) {
        Err(e) => e,
        Ok(config) => panic!("accepted: {config:?}"),
    };

    // A typo in a key must not quietly widen or drop a rule.
    for typo in [
        "[[rule]]\nmatch = { tools = \"x\" }\naction = \"deny\"",
        "[[rule]]\nmtach = { tool = \"x\" }\naction = \"deny\"",
        "[[rule]]\nmatch = { tool = \"x\" }\naction = \"block\"",
        "[[rule]]\nmatch = { tool = \"x\" }",
        "[[rules]]\naction = \"deny\"",
        "[budget]\nmax_token = 5",
    ] {
        assert!(matches!(invalid(typo), PolicyError::Parse(_)), "{typo}");
    }

    for (text, why) in [
        (
            "[[rule]]\nname = \"a\"\naction = \"allow\"\n[[rule]]\nname = \"a\"\naction = \"deny\"",
            "same name",
        ),
        (
            "[[rule]]\nmatch = { argv0 = [] }\naction = \"deny\"",
            "empty pattern",
        ),
        (
            "[[rule]]\nmatch = { tool = \"\" }\naction = \"deny\"",
            "empty pattern",
        ),
        (
            "[[rule]]\naction = \"allow\"\non_timeout = \"deny\"",
            "on_timeout",
        ),
    ] {
        match invalid(text) {
            PolicyError::InvalidRule { reason, .. } => assert!(reason.contains(why), "{reason}"),
            other => panic!("expected InvalidRule for {text}, got {other}"),
        }
    }
}

#[test]
fn snapshot_is_the_file_byte_for_byte() {
    let policy = RulePolicy::from_toml(EXAMPLE).expect("example policy");
    assert_eq!(policy.snapshot().as_deref(), Some(EXAMPLE));

    // Loading the snapshot gives the same policy back.
    let reloaded = RulePolicy::from_toml(&policy.snapshot().expect("snapshot")).expect("reload");
    assert_eq!(reloaded, policy);

    assert_eq!(RulePolicy::default().snapshot().as_deref(), Some(""));
    assert_eq!(AllowAll.snapshot(), None);
}

#[test]
fn budget_table_becomes_the_run_limits() {
    let policy = RulePolicy::from_toml(EXAMPLE).expect("example policy");
    assert_eq!(
        policy.limits(),
        Limits {
            max_tokens: Some(2_000_000),
            max_usd: Some(5.0),
            max_wall_secs: Some(2400),
            max_tool_calls: Some(400),
        }
    );

    // No table, or a policy with no file, means no limits.
    assert_eq!(RulePolicy::default().limits(), Limits::default());
    assert_eq!(AllowAll.limits(), Limits::default());

    // A whole number is fine for dollars; a negative one is not.
    let whole = RulePolicy::from_toml("[budget]\nmax_usd = 5").expect("policy");
    assert_eq!(whole.limits().max_usd, Some(5.0));
    assert!(matches!(
        PolicyConfig::from_toml("[budget]\nmax_usd = -1.0"),
        Err(PolicyError::InvalidBudget { key: "max_usd", .. })
    ));
}

#[test]
fn server_matcher_covers_a_whole_server() {
    let policy = RulePolicy::from_toml(
        r#"
[[rule]]
name = "no-search"
match = { server = "search" }
action = "deny"
reason = "no search in this run"

[[rule]]
name = "fs-ok"
match = { server = "fs-*" }
action = "allow"

[[rule]]
name = "builtin"
match = { server = "sandbox", tool = "sandbox.read_file" }
action = "allow"
"#,
    )
    .expect("parses");
    assert_eq!(
        outcome(&policy, &call("search", "web", json!({}))),
        ("deny", rule("no-search"))
    );
    assert_eq!(
        outcome(&policy, &call("fs-local", "list", json!({}))),
        ("allow", rule("fs-ok"))
    );
    assert_eq!(
        outcome(&policy, &call("sandbox", "read_file", json!({"path": "a"}))),
        ("allow", rule("builtin"))
    );
    // Nothing matches an unknown server, so the default `ask` applies.
    assert_eq!(outcome(&policy, &call("other", "x", json!({}))).0, "ask");
}

#[test]
fn mcp_server_tables_parse_and_validate() {
    use tether_policy::config::{McpTransport, PolicyConfig};

    let config = PolicyConfig::from_toml(
        r#"
[[mcp_server]]
id = "fs"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
env = ["PATH"]

[[mcp_server]]
id = "search"
transport = "http"
url = "http://127.0.0.1:8931/mcp"
headers = { Authorization = "Bearer x" }
timeout_secs = 5
"#,
    )
    .expect("parses");
    assert_eq!(config.mcp_servers.len(), 2);
    assert_eq!(config.mcp_servers[0].transport, McpTransport::Stdio);
    assert_eq!(config.mcp_servers[0].timeout_secs, 60);
    assert_eq!(config.mcp_servers[1].transport, McpTransport::Http);
    assert_eq!(config.mcp_servers[1].timeout_secs, 5);

    for (bad, expect) in [
        ("[[mcp_server]]\nid = \"sandbox\"\ntransport = \"stdio\"\ncommand = \"x\"", "built-in"),
        ("[[mcp_server]]\nid = \"a b\"\ntransport = \"stdio\"\ncommand = \"x\"", "letters"),
        ("[[mcp_server]]\nid = \"a\"\ntransport = \"stdio\"", "needs `command`"),
        ("[[mcp_server]]\nid = \"a\"\ntransport = \"http\"\nurl = \"ftp://x\"", "http://"),
        ("[[mcp_server]]\nid = \"a\"\ntransport = \"http\"\nurl = \"http://x\"\ncommand = \"c\"", "for transport = \"stdio\""),
        ("[[mcp_server]]\nid = \"a\"\ntransport = \"stdio\"\ncommand = \"x\"\n[[mcp_server]]\nid = \"a\"\ntransport = \"stdio\"\ncommand = \"y\"", "same id"),
        ("[[mcp_server]]\nid = \"a\"\ntransport = \"stdio\"\ncommand = \"x\"\ntimeout_secs = 0", "at least 1"),
        ("[[mcp_server]]\nid = \"a\"\ntransport = \"stdio\"\ncommand = \"x\"\nbogus = 1", "bogus"),
    ] {
        let err = PolicyConfig::from_toml(bad).expect_err(bad).to_string();
        assert!(err.contains(expect), "{bad}\n-> {err}");
    }
}

/// A policy that denies a secret directory and `rm`, then allows the rest.
fn guarded() -> RulePolicy {
    RulePolicy::from_toml(
        r#"
        [[rule]]
        name = "no-secrets"
        match = { path = ["secrets", "secrets/**"] }
        action = "deny"

        [[rule]]
        name = "no-rm"
        match = { tool = "sandbox.exec", argv0 = "rm" }
        action = "deny"

        [[rule]]
        name = "src-only"
        match = { tool = "sandbox.write_file", path = "src/**" }
        action = "allow"

        [[rule]]
        name = "rest"
        match = { server = "*" }
        action = "allow"
        "#,
    )
    .expect("policy")
}

#[test]
fn a_deny_ignores_letter_case_and_an_allow_does_not() {
    let policy = guarded();
    // On a filesystem that ignores case these name the denied things.
    assert_eq!(
        outcome(&policy, &write("SECRETS/key.txt")),
        ("deny", rule("no-secrets"))
    );
    assert_eq!(
        outcome(&policy, &exec(&["RM", "-rf", "."])),
        ("deny", rule("no-rm"))
    );
    assert_eq!(
        outcome(&policy, &exec(&["/bin/Rm", "x"])),
        ("deny", rule("no-rm"))
    );
    // An allow matches as written: `SRC/` is not `src/`, so the narrow
    // allow is not what lets it through.
    assert_eq!(
        outcome(&policy, &write("src/a.rs")),
        ("allow", rule("src-only"))
    );
    assert_eq!(
        outcome(&policy, &write("SRC/a.rs")),
        ("allow", rule("rest"))
    );
}

#[test]
fn a_path_that_climbs_out_cannot_skip_a_deny() {
    let policy = guarded();
    // Nobody can say what `../workspace/secrets` names, so the deny on a
    // path takes it; before, no path rule matched and the last allow did.
    assert_eq!(
        outcome(&policy, &write("../workspace/secrets/key.txt")),
        ("deny", rule("no-secrets"))
    );
    assert_eq!(
        outcome(
            &policy,
            &call("sandbox", "list_dir", json!({"path": "../.."}))
        ),
        ("deny", rule("no-secrets"))
    );
    // A path that goes down and back up without leaving is just a path.
    assert_eq!(
        outcome(&policy, &write("src/../src/a.rs")),
        ("allow", rule("src-only"))
    );
    assert_eq!(
        outcome(&policy, &write("src/../secrets/key.txt")),
        ("deny", rule("no-secrets"))
    );
}

#[test]
fn a_tool_pattern_covers_any_name_a_server_gives_a_tool() {
    let policy = RulePolicy::from_toml(
        r#"
        [[rule]]
        name = "no-evil"
        match = { tool = "evil.*" }
        action = "deny"

        [[rule]]
        name = "rest"
        match = { tool = "**" }
        action = "allow"
        "#,
    )
    .expect("policy");
    assert_eq!(
        outcome(&policy, &call("evil", "read_note", json!({}))),
        ("deny", rule("no-evil"))
    );
    // A `/` in a tool name is not a directory.
    assert_eq!(
        outcome(&policy, &call("evil", "admin/delete_all", json!({}))),
        ("deny", rule("no-evil"))
    );
    assert_eq!(
        outcome(&policy, &call("fine", "admin/delete_all", json!({}))),
        ("allow", rule("rest"))
    );
}

#[test]
fn a_pattern_too_long_to_match_quickly_is_refused() {
    let long = "a".repeat(2000);
    let error = PolicyConfig::from_toml(&format!(
        "[[rule]]\nmatch = {{ path = \"{long}\" }}\naction = \"deny\"\n"
    ))
    .expect_err("too long");
    assert!(error.to_string().contains("longer than"), "{error}");
}

#[test]
fn the_ask_prompt_shows_the_path_first_and_says_what_it_left_out() {
    let policy = RulePolicy::from_toml(
        "[[rule]]\nname = \"ask-all\"\nmatch = { server = \"*\" }\naction = \"ask\"\n",
    )
    .expect("policy");
    let hidden = call(
        "sandbox",
        "write_file",
        json!({"content": "x".repeat(5000), "path": "secrets/key.txt"}),
    );
    let Decision::Ask { prompt } = decide(&policy, &hidden).decision else {
        panic!("expected an ask");
    };
    // `content` sorts before `path`; the prompt must not let it hide the path.
    assert!(
        prompt.starts_with("Allow sandbox.write_file {\"path\":\"secrets/key.txt\""),
        "{prompt}"
    );
    assert!(prompt.contains("more characters not shown"), "{prompt}");

    let short = call("sandbox", "exec", json!({"argv": ["ls"]}));
    let Decision::Ask { prompt } = decide(&policy, &short).decision else {
        panic!("expected an ask");
    };
    assert_eq!(prompt, "Allow sandbox.exec {\"argv\":[\"ls\"]}?");
}

#[test]
fn the_idle_timeout_has_a_default_and_can_be_set_or_turned_off() {
    use std::time::Duration;
    let idle = |toml: &str| {
        PolicyConfig::from_toml(toml)
            .expect("policy")
            .budget
            .idle_timeout()
    };
    assert_eq!(idle(""), Some(Duration::from_secs(600)));
    assert_eq!(
        idle("[budget]\nidle_timeout_secs = 30\n"),
        Some(Duration::from_secs(30))
    );
    assert_eq!(idle("[budget]\nidle_timeout_secs = 0\n"), None);
}
