use serde_json::{json, Value};
use tether_policy::config::{Action, OnTimeout};
use tether_policy::{AllowAll, Policy, PolicyConfig, PolicyError, RulePolicy, RunCtx, Verdict};
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
    // A name the registry prefixed on a collision is not prefixed twice.
    assert_eq!(
        outcome(&policy, &call("github", "github.create_issue", json!({}))),
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
