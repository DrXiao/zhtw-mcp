S2T_DATA := src/engine/s2t_data.rs
OPENCC_DICT_DIR := data/opencc

# What make tracks, because $(S2T_DATA) cannot record that the generator ran:
# the generator rewrites it only when the tables change, so after any Cargo.toml
# edit its mtime stays behind that prerequisite and every later `make` reruns
# the generator and rustfmt for nothing.  Touching $(S2T_DATA) instead would
# defeat the point, since cargo fingerprints by mtime and would rebuild the
# crate for a file whose bytes did not change.  The stamp lives beside the
# dictionary cache so `distclean` takes it too.
S2T_STAMP := $(OPENCC_DICT_DIR)/.tables-generated

# The stamp stands in for the generated file, so a hand-deleted s2t_data.rs has
# to invalidate it: otherwise the stamp still looks current and the build fails
# on a file nothing would regenerate.  `distclean` removes both, so this covers
# only the by-hand case.
ifeq ($(wildcard $(S2T_DATA)),)
.PHONY: $(S2T_STAMP)
endif

all: $(S2T_STAMP)
	cargo build --release

# gen-s2t-tables.py handles downloading from GitHub + code generation.
# Cargo.toml is a prerequisite too: the OpenCC commit is pinned in its
# [package.metadata.opencc] table, so changing which dictionaries we build
# from means editing one of these two files.  Make cannot depend on a single
# table inside a file, so an unrelated manifest edit reruns the generator;
# it rewrites s2t_data.rs only when the tables change, so nothing rebuilds.
$(S2T_STAMP): scripts/gen-s2t-tables.py Cargo.toml
	python3 scripts/gen-s2t-tables.py
	rustfmt $(S2T_DATA)
	@touch $@

clean:
	cargo clean

distclean: clean
	rm -f $(S2T_DATA)
	rm -rf $(OPENCC_DICT_DIR)

check: $(S2T_STAMP)
	cargo test
	cargo clippy --all-targets -- -D warnings
# The default feature set is not the only one that ships: the browser extension
# builds the library with browser-wasm and no native, and that configuration
# used to accumulate dead-code warnings nothing gated. Lint the two non-default
# shapes as well. Library only, since the binary needs native.
	cargo clippy --lib --no-default-features -- -D warnings
	cargo clippy --lib --no-default-features --features browser-wasm -- -D warnings
	cargo fmt --check
	python3 scripts/check-ruleset.py --lint

check-size: all
	@SIZE=$$(wc -c < target/release/zhtw-mcp | tr -d ' '); \
	MAX=20971520; \
	if [ "$$SIZE" -gt "$$MAX" ]; then \
		echo "FAIL: release binary $$SIZE bytes exceeds 20 MiB budget ($$MAX)"; \
		exit 1; \
	else \
		echo "OK: release binary $$SIZE bytes (budget: $$MAX)"; \
	fi

indent: $(S2T_STAMP)
	cargo fmt
	python3 scripts/check-ruleset.py
	python3 scripts/check-ruleset.py --lint
	black scripts/*.py

corpus: $(S2T_STAMP)
	cargo test --test corpus-evaluation -- --nocapture

.PHONY: all clean distclean check check-size corpus indent install uninstall status

install: all
	@./scripts/deploy.sh install

uninstall:
	@./scripts/deploy.sh uninstall

status:
	@./scripts/deploy.sh status
