.PHONY: release

release:
	cargo build --release
	sudo setcap cap_net_bind_service,cap_net_raw,cap_sys_ptrace+ep target/release/mdrv-ds
	@echo "build + setcap done"
