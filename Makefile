# fastbrew build and install helpers.
#
#   make build            release build into target/release/fastbrew
#   make install          copy the binary into a directory on your PATH
#   make install BINDIR=/usr/local/bin
#   make uninstall        remove the installed binary
#   make test             unit tests + integration tests in a sandbox
#   make check            rustfmt and clippy
#   make bench            A/B benchmark against a sandboxed Homebrew
#
# BINDIR defaults to the first directory in this list that exists, is on
# your PATH and is writable: ~/.cargo/bin, ~/.local/bin, /opt/homebrew/bin,
# /usr/local/bin. Pass BINDIR=... to choose another one.

CARGO  ?= cargo
BIN     = target/release/fastbrew
CANDIDATES = $(HOME)/.cargo/bin $(HOME)/.local/bin /opt/homebrew/bin /usr/local/bin

BINDIR ?= $(shell for d in $(CANDIDATES); do case ":$$PATH:" in *":$$d:"*) if [ -d "$$d" ] && [ -w "$$d" ]; then echo "$$d"; break; fi ;; esac; done)

.PHONY: build install uninstall test check bench clean

build:
	$(CARGO) build --release

install: build
	@if [ -z "$(BINDIR)" ]; then \
		echo "No writable bin directory on your PATH found among: $(CANDIDATES)"; \
		echo "Run: make install BINDIR=<directory on your PATH>"; exit 1; fi
	install -d "$(BINDIR)"
	install -m 755 "$(BIN)" "$(BINDIR)/fastbrew"
	@echo "Installed $(BINDIR)/fastbrew"
	@case ":$$PATH:" in *":$(BINDIR):"*) ;; *) echo "Note: $(BINDIR) is not on your PATH." ;; esac

uninstall:
	@if [ -z "$(BINDIR)" ]; then echo "Pass BINDIR=<directory> to uninstall"; exit 1; fi
	rm -f "$(BINDIR)/fastbrew"
	@echo "Removed $(BINDIR)/fastbrew"

test:
	$(CARGO) test --lib
	scripts/sandbox.sh test

check:
	$(CARGO) fmt --all --check
	$(CARGO) clippy --all-targets -- -D warnings

bench:
	scripts/bench.sh

clean:
	$(CARGO) clean
