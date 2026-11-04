# The pty harness: mock provider, tmux, and a virtual terminal.
# Needs python3 and tmux. Extra arguments go through ARGS:
#   make harness ARGS='--scroll 400 "fetch:plain"'
#   make harness ARGS='--expect ok "text:ok"'

.PHONY: harness

harness:
	cargo build
	./harness.py $(ARGS)
