# Ale, My Eyes! - Android

Android application for the Ale, My Eyes! visual assistant.

## Overview

This repository contains the Android version of the Ale, My Eyes! application, including:

- **ale-gui**: Graphical user interface with camera capture
- **ale-core**: Core library (shared via git submodule from [ale-my-eyes-core](https://github.com/risalydevclub/ale-my-eyes-core))

## Features

- Voice-activated visual assistance
- Camera capture and analysis
- Text-to-speech responses
- Cloud API integration (OpenAI GPT-4o, Whisper, TTS)

## Building

### Prerequisites

- Rust toolchain (stable)
- Android NDK (version 27.3.13750724)
- Java 17 (for Android SDK)
- cargo-apk: `cargo install cargo-apk`

### Build

```bash
# Initialize submodule
git submodule update --init --recursive

# Add Android targets
rustup target add aarch64-linux-android
rustup target add armv7-linux-androideabi

# Build APKs
./scripts/package-android.sh
```

## Usage

Install the APK on your Android device:
```bash
adb install ale-my-eyes-arm64.apk
```

## Related Projects

- [ale-my-eyes-core](https://github.com/risalydevclub/ale-my-eyes-core) - Core library
- [ale-my-eyes-pc](https://github.com/risalydevclub/ale-my-eyes-pc) - Desktop application

## License

MIT
