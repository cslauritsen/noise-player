# syntax=docker/dockerfile:1

# Bookworm's glibc (2.36) keeps the binary runnable on Bookworm and Trixie Pi OS.
ARG DEBIAN=bookworm

FROM rust:1-${DEBIAN} AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends libasound2-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/noise-player /usr/local/bin/

# Just the binary, for `docker build --target binary --output <dir> .`
FROM scratch AS binary
COPY --from=builder /usr/local/bin/noise-player /

FROM debian:${DEBIAN}-slim
# alsa-utils gives `aplay -l` / `speaker-test` for checking the USB speaker.
RUN apt-get update \
    && apt-get install -y --no-install-recommends libasound2 alsa-utils \
    && rm -rf /var/lib/apt/lists/*
# Debian's `audio` group is GID 29, the same as on Raspberry Pi OS, so this user
# can open the host's /dev/snd devices.
RUN useradd --system --no-create-home --groups audio noise
COPY --from=builder /usr/local/bin/noise-player /usr/local/bin/
USER noise
ENTRYPOINT ["noise-player"]
CMD ["run"]
