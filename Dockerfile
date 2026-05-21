# ── Stage 1: base ────────────────────────────────────────────────────────────
# OS packages + Rust toolchain.  Rebuilt only when the package list or the
# upstream Rocky Linux 8 image changes.
FROM rockylinux:8 AS base

RUN dnf install -y epel-release 'dnf-command(config-manager)' && \
    dnf config-manager --set-enabled powertools && \
    dnf update -y && \
    dnf install -y \
        # Build tools (cmake required by the bundled hdf5-src crate)
        gcc gcc-c++ make cmake pkg-config curl git \
        # zlib (hdf5-metno "zlib" feature links against system libz)
        zlib-devel \
        # X11 windowing — winit uses x11-dl (dlopen at runtime, headers at build time)
        libxkbcommon-devel \
        libX11-devel libXrandr-devel libXi-devel \
        libXcursor-devel libXinerama-devel \
        # Wayland — winit compiles both X11 and Wayland backends
        wayland-devel \
        # OpenGL headers — egui/eframe GPU texture upload
        mesa-libGL-devel mesa-libEGL-devel \
        # GTK3 — rfd file-dialog backend on Linux
        gtk3-devel \
        # OpenSSL — reqwest TLS (loaded via dlopen; headers needed at compile time)
        openssl-devel \
    && dnf clean all

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"


# ── Stage 2: deps ─────────────────────────────────────────────────────────────
# Compile every Cargo dependency with a stub binary.  This layer is invalidated
# only when Cargo.toml or Cargo.lock changes, not when application source
# files change.  The expensive HDF5 cmake build lives here.
FROM base AS deps

WORKDIR /src
COPY Cargo.toml Cargo.lock ./

# build.rs reads git tags — stub it so this layer doesn't need the .git tree.
RUN echo 'fn main() {}' > build.rs && \
    mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release && \
    # Remove the stub binary so cargo rebuilds the real app in the next stage.
    rm -f target/release/pumpkin


# ── Stage 3: builder ──────────────────────────────────────────────────────────
# Compile only the application crate; all dependencies are already compiled and
# cached in the layer above.
FROM deps AS builder

COPY . .

# Touch the files that were stubbed in the deps stage so cargo's fingerprint
# check always triggers a recompile of the application crate and build.rs,
# regardless of whether Docker COPY preserves original timestamps.
RUN touch build.rs src/main.rs && cargo build --release


# ── Stage 4: export ───────────────────────────────────────────────────────────
# Scratch image containing only the release binary.  Used with
# "docker build --output" to copy the binary directly to the host without
# serialising the multi-GB target/ tree into a Docker image layer.
FROM scratch AS export
COPY --from=builder /src/target/release/pumpkin /pumpkin
