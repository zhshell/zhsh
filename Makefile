.PHONY: build install test lint ci deb rpm rpm-tool packages release-check release-check-tag

CARGO_GENERATE_RPM_VERSION := 0.21.0

OFFICIAL_CODECS := \
	zhsh/assets/llm-codecs/openai@0.3.0.zhcodec \
	zhsh/assets/llm-codecs/anthropic@0.3.0.zhcodec

build:
	cargo build --release --quiet --package zhsh

install:
	test -x target/release/zhsh
	install -d -m755 $(DESTDIR)/usr/bin
	install -m755 target/release/zhsh $(DESTDIR)/usr/bin/zhsh
	install -d -m755 $(DESTDIR)/usr/lib/zhsh/plugins/llm
	install -m644 $(OFFICIAL_CODECS) $(DESTDIR)/usr/lib/zhsh/plugins/llm/
	install -d -m755 $(DESTDIR)/usr/share/man/man1
	install -m644 packaging/man/zhsh.1 $(DESTDIR)/usr/share/man/man1/zhsh.1
	install -d -m755 $(DESTDIR)/usr/share/doc/zhsh
	install -m644 README.md $(DESTDIR)/usr/share/doc/zhsh/README.md
	install -m644 LICENSE $(DESTDIR)/usr/share/doc/zhsh/LICENSE

test:
	cargo test --package zhsh --quiet

lint:
	cargo clippy --package zhsh --quiet --all-targets --all-features

ci:
	cargo fmt --all -- --check
	cargo check --package zhsh --locked --all-targets --all-features --quiet
	cargo test --package zhsh --locked --quiet
	cargo clippy --package zhsh --locked --all-targets --all-features --quiet -- -D warnings

# Requires: cargo install cargo-deb --locked
deb: build
	cargo deb --package zhsh --no-build --quiet

rpm-tool:
	@if ! cargo-generate-rpm --version 2>/dev/null | grep -qx "cargo-generate-rpm $(CARGO_GENERATE_RPM_VERSION)"; then \
		cargo install cargo-generate-rpm --version $(CARGO_GENERATE_RPM_VERSION) --locked; \
	fi

rpm: build rpm-tool
	scripts/build-rpm.sh

packages: deb rpm

release-check:
	scripts/release-check.sh

release-check-tag:
	scripts/check-release-tag.sh
