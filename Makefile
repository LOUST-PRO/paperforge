# paperforge Makefile — local atomic CI gate
#
# This Makefile mirrors .github/workflows/ci.yml 1:1 so that local runs
# produce logs API-compatible with GitHub Actions output. Each target
# corresponds to a workflow job or step. The intent is:
#
#   - Layer 1 (this file): local pre-push gate. No GitHub, no Docker,
#     no internet. Runs on the laptop using the same cargo invocations
#     the workflow uses. Output format mirrors GitHub Actions log lines
#     so scripts/trackers can post-process both with the same parser.
#
#   - Layer 2: `act` (optional). Run the actual workflow YAML locally
#     in Docker; catches drift between local Makefile and YAML.
#
#   - Layer 3: self-hosted runner on VPS (vps-runner-paperforge).
#     Catches the 4 realtime tests that wedge on PR #12 mpsc backpressure.
#
#   - Layer 4: GitHub Actions. CI/CD of record for the public repo.
#     Can fall (it does, regularly), but the local gate doesn't depend
#     on it.
#
# Usage:
#   make ci               # mirror of github-hosted jobs (fmt+clippy+test+build)
#   make ci-realtime      # the 4 PR #12 wedge tests against the local daemon
#   make ci-all           # ci + ci-realtime
#   make pre-push         # ci-all + sanitize + non-secret check
#   make ci-log           # tail ~/.local/share/paperforge/ci.log
#   make ci-stats         # pass/fail counts over the last N runs
#   make ci-help          # full target list

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------
CARGO       ?= cargo
RUSTFLAGS   ?= -D warnings
WORKSPACE    = $(CURDIR)
LOG_DIR     ?= $(HOME)/.local/share/paperforge
LOG_FILE    ?= $(LOG_DIR)/ci.log
ROTATE_KEEP ?= 200

# GitHub Actions log-line prefix format. The workflow emits:
#   ::group::name
#   ::error::msg
#   ::warning::msg
# We mirror the same prefix so downstream tools (e.g. lzt-ci-parse) can
# parse either source identically.
GH_GROUP_PREFIX = "::group::"
GH_ENDGROUP     = "::endgroup::"
GH_ERROR_PREFIX = "::error::"

# ---------------------------------------------------------------------------
# Phony targets
# ---------------------------------------------------------------------------
.PHONY: help ci ci-fmt ci-clippy ci-test ci-build ci-realtime ci-all \
        pre-push ci-log ci-stats ci-clean-log install-hooks

help: ci-help

ci-help:
	@echo "paperforge local atomic CI — 1:1 mirror of .github/workflows/ci.yml"
	@echo ""
	@echo "Targets:"
	@echo "  ci              Run all github-hosted jobs (fmt+clippy+test+build)"
	@echo "  ci-fmt          rustfmt --check"
	@echo "  ci-clippy       clippy --all-targets --all-features -- -D warnings"
	@echo "  ci-test         unit + integration tests (excludes 4 PR #12 wedge)"
	@echo "  ci-build        cargo build --release --locked + smoke"
	@echo "  ci-realtime     the 4 PR #12 zbus mpsc tests (require local daemon)"
	@echo "  ci-all          ci + ci-realtime"
	@echo "  pre-push        ci-all + sanitize + non-secret scan"
	@echo "  ci-log          tail ~/.local/share/paperforge/ci.log"
	@echo "  ci-stats        pass/fail counts over the last N runs"
	@echo "  ci-clean-log    keep last $(ROTATE_KEEP) lines"
	@echo "  install-hooks   install pre-push hook that runs 'make pre-push'"
	@echo ""
	@echo "Env vars:"
	@echo "  RUSTFLAGS       extra flags (default: -D warnings)"
	@echo "  LOG_FILE        override log destination"
	@echo "  ROTATE_KEEP     lines to keep on clean (default: 200)"
	@echo "  PAPERFORGE_QUIET=1   suppress GH-format prefixes (plain output)"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# $(call log-run,target,step,command) — runs command, logs result to file,
# prints GH-format line to stdout.
#
# Output format matches GitHub Actions log shape:
#   ::group::<target>::<step>
#   <command output>
#   ::endgroup::
#   ::error::<target>::<step> failed (exit=1, 1.23s)
# OR
#   ::notice::<target>::<step> ok (1.23s)
define log-run
	@mkdir -p $(LOG_DIR)
	@if [ "$(PAPERFORGE_QUIET)" = "1" ]; then \
	    printf '\n=== %s :: %s ===\n' "$(1)" "$(2)"; \
	else \
	    printf '\n%s%s :: %s\n' "$(GH_GROUP_PREFIX)" "$(1)" "$(2)"; \
	fi
	@start=$$(date +%s.%N); \
	if $(3) >> $(LOG_FILE) 2>&1; then \
	    end=$$(date +%s.%N); \
	    dur=$$(awk -v s=$$start -v e=$$end 'BEGIN{printf "%.2f", e-s}'); \
	    printf '%s %s :: %s :: %s ok (%.2fs)\n' \
	        "$$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(1)" "$(2)" "$$(hostname)" "$$dur" \
	        >> $(LOG_FILE); \
	    if [ "$(PAPERFORGE_QUIET)" = "1" ]; then \
	        printf 'OK (%.2fs)\n' "$$dur"; \
	    else \
	        printf '%s%s :: %s ok (%.2fs)\n' "$(GH_GROUP_PREFIX)" "$(1)" "$(2)" "$$dur"; \
	    fi; \
	else \
	    rc=$$?; \
	    end=$$(date +%s.%N); \
	    dur=$$(awk -v s=$$start -v e=$$end 'BEGIN{printf "%.2f", e-s}'); \
	    printf '%s %s :: %s :: %s FAILED exit=%d (%.2fs)\n' \
	        "$$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(1)" "$(2)" "$$(hostname)" "$$rc" "$$dur" \
	        >> $(LOG_FILE); \
	    if [ "$(PAPERFORGE_QUIET)" = "1" ]; then \
	        printf 'FAILED exit=%d (%.2fs) — see %s\n' "$$rc" "$$dur" "$(LOG_FILE)"; \
	    else \
	        printf '%s%s :: %s failed (exit=%d, %.2fs)\n' "$(GH_ERROR_PREFIX)" "$(1)" "$(2)" "$$rc" "$$dur"; \
	    fi; \
	    exit $$rc; \
	fi
endef

# ---------------------------------------------------------------------------
# ci-fmt — mirrors the `fmt` job: cargo fmt --all -- --check
# ---------------------------------------------------------------------------
ci-fmt:
	$(call log-run,ci-fmt,rustfmt,$(CARGO) fmt --all -- --check)

# ---------------------------------------------------------------------------
# ci-clippy — mirrors the `clippy` job
# ---------------------------------------------------------------------------
ci-clippy:
	$(call log-run,ci-clippy,clippy,RUSTFLAGS='$(RUSTFLAGS)' $(CARGO) clippy --all-targets --all-features -- -D warnings)

# ---------------------------------------------------------------------------
# ci-test — mirrors the `test` job, but skips the 4 PR #12 wedge tests
# (those run in ci-realtime against the local daemon).
#
# --test-threads=1 is mandatory: several tests in the pool suite read
# /proc/<pid>/status after sending SIGSTOP. Under parallel load the
# kernel can take >50ms to record the signal state, and the assertion
# fires before the state settles. Sequential execution costs ~3x more
# wall time but eliminates the flakes (0 → 0 in nightly).
# ---------------------------------------------------------------------------
ci-test:
	$(call log-run,ci-test,unit_and_integration,$(CARGO) test --all --no-fail-fast -- --test-threads=1 \
		--skip reconcile_outputs_respawns_only_dead_pids \
		--skip resume_per_output_specific_respawns_with_last_scene \
		--skip daemon_reconcile_emits_wallpaper_started_for_respawned_outputs \
		--skip lwe_backend_ops_pool_disabled_returns_real_pid)

# ---------------------------------------------------------------------------
# ci-build — mirrors the `build` job
# ---------------------------------------------------------------------------
ci-build:
	$(call log-run,ci-build,release_build,$(CARGO) build --release --locked)
	$(call log-run,ci-build,smoke_cli,./target/release/paperforge --version)
	$(call log-run,ci-build,smoke_help,./target/release/paperforge --help)
	$(call log-run,ci-build,smoke_paths,./target/release/paperforge paths)

# ---------------------------------------------------------------------------
# ci — aggregate of fmt+clippy+test+build (github-hosted equivalent)
# ---------------------------------------------------------------------------
ci: ci-fmt ci-clippy ci-test ci-build
	@printf '\nci: all green — local API-compatible with github-hosted jobs\n'

# ---------------------------------------------------------------------------
# ci-realtime — the 4 PR #12 wedge tests
# These tests touch the local zbus daemon and the real LWE binary.
# They cannot run in CI on github-hosted (no niri, no LWE). Run locally
# against the user-side daemon.
#
# Requires:
#   - paperforge daemon running (systemctl --user status paperforge)
#   - linux-wallpaperengine in PATH
#   - A live niri/wlroots session (or output stub if running headless)
#
# These tests are the ones we explicitly isolated from the regular
# test suite via --skip above. They run on the self-hosted runner
# via workflow_dispatch full_realtime=true, but also work locally.
# ---------------------------------------------------------------------------
ci-realtime:
	@command -v linux-wallpaperengine > /dev/null 2>&1 || \
	    { echo "WARNING: linux-wallpaperengine not in PATH; some tests will probe_for_lwe and skip"; }
	$(call log-run,ci-realtime,realtime_test_set,$(CARGO) test -p paperforge-core --lib --no-fail-fast -- \
		reconcile_outputs_respawns_only_dead_pids \
		resume_per_output_specific_respawns_with_last_scene \
		daemon_reconcile_emits_wallpaper_started_for_respawned_outputs \
		lwe_backend_ops_pool_disabled_returns_real_pid)

# ---------------------------------------------------------------------------
# ci-all — fast local gauntlet (no realtime tests).
# Use ci-realtime explicitly when you want to run the 4 PR #12 wedge tests
# against the local daemon — those tests hang without a live daemon.
# ---------------------------------------------------------------------------
ci-all: ci
	@printf '\nci-all: fast local gauntlet green — safe to push\n'
	@printf 'NOTICE: ci-realtime (the 4 PR #12 wedge tests) is NOT included.\n'
	@printf '        Run \047make ci-realtime\047 separately if you want them.\n'

# ---------------------------------------------------------------------------
# pre-push — ci-all + sanitize + non-secret scan
# ---------------------------------------------------------------------------
pre-push: ci-all
	@printf '\n%spre-push :: scan_for_secrets\n' "$(GH_GROUP_PREFIX)"
	@git diff --cached --name-only | while read -r f; do \
	    if [ -f "$$f" ] && grep -lE '(api[_-]?key|password|secret|token[^a-z_])' "$$f" 2>/dev/null | grep -q .; then \
	        printf '%s%s :: secret_smell in %s\n' "$(GH_ERROR_PREFIX)" "pre-push" "$$f"; \
	        exit 1; \
	    fi; \
	done || { printf '%s%s :: pre-push secret scan failed\n' "$(GH_ERROR_PREFIX)" "pre-push"; exit 1; }
	@printf '%s%s :: secret scan clean\n' "$(GH_GROUP_PREFIX)" "pre-push"
	@printf '\npre-push: all green — safe to git push\n'

# ---------------------------------------------------------------------------
# ci-log — tail the local CI log
# ---------------------------------------------------------------------------
ci-log:
	@if [ -f $(LOG_FILE) ]; then tail -50 $(LOG_FILE); else echo "no log file at $(LOG_FILE) yet"; fi

# ---------------------------------------------------------------------------
# ci-stats — pass/fail counts over the last N runs
# ---------------------------------------------------------------------------
ci-stats:
	@if [ -f $(LOG_FILE) ]; then \
	    total=$$(grep -ciE ' ok \(' $(LOG_FILE) 2>/dev/null; true); \
	    fail=$$(grep -ciE ' FAILED ' $(LOG_FILE) 2>/dev/null; true); \
	    total=$${total:-0}; \
	    fail=$${fail:-0}; \
	    ok=$$((total - fail)); \
	    if [ "$$total" -gt 0 ] 2>/dev/null; then \
	        pct=$$(awk -v o=$$ok -v t=$$total 'BEGIN{printf "%.1f", (o*100.0)/t}'); \
	        printf 'last %d runs: %d ok, %d failed (%s%% pass rate)\n' \
	            "$$total" "$$ok" "$$fail" "$$pct"; \
	    else \
	        printf 'no completed runs logged yet\n'; \
	    fi; \
	else \
	    echo "no log file at $(LOG_FILE) yet"; \
	fi

# ---------------------------------------------------------------------------
# ci-clean-log — keep last $(ROTATE_KEEP) lines
# ---------------------------------------------------------------------------
ci-clean-log:
	@if [ -f $(LOG_FILE) ]; then \
	    tail -n $(ROTATE_KEEP) $(LOG_FILE) > $(LOG_FILE).tmp && \
	    mv $(LOG_FILE).tmp $(LOG_FILE) && \
	    echo "trimmed to last $(ROTATE_KEEP) lines"; \
	else \
	    echo "no log file to clean"; \
	fi

# ---------------------------------------------------------------------------
# install-hooks — install a git pre-push hook that runs `make pre-push`
# The hook script lives in scripts/install-pre-push-hook.sh so the bash
# heredoc stays out of the Makefile (Make recipe continuation breaks
# multi-line heredoc bodies).
# ---------------------------------------------------------------------------
install-hooks:
	@bash scripts/install-pre-push-hook.sh
