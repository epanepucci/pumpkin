FROM rockylinux:8

# Enable EPEL and PowerTools
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

# Install Rust toolchain
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /src
COPY . .

# hdf5-metno is built with features = ["static", "zlib"]:
#   - "static"  → hdf5-src compiles HDF5 1.10.7 from bundled source via cmake
#   - "zlib"    → enables zlib filter support inside HDF5, links system libz
# Result: no libhdf5.so dependency in the binary.
RUN cargo build --release
