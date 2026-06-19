# Mic Volume Fix

Small Windows tray utility that keeps the default microphone unmuted and at 100% volume.

It watches Core Audio endpoint-volume changes, restores the mic immediately when another app lowers or mutes it, and stays lightweight with bounded event queues, Efficiency Mode, and low idle CPU use.

## Build

```sh
cargo build --target x86_64-pc-windows-gnu --release --locked
```

## License

MIT. See [LICENSE](LICENSE).
