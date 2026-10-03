# The displayless smoke's runtime: the dev image plus sway driven headless
# (wlroots), a session bus, and the software GL/Vulkan stack gpui's
# renderer reaches for once an output exists. Not part of the build path;
# see scripts/displayless-smoke.sh. niri itself cannot run here (it needs
# a TTY seat), so sway stands in for the compositor; where the two differ
# the script says so inline.
FROM localhost/kuma-dev-rust
RUN dnf -y install sway dbus-daemon jq wayland-utils mesa-libEGL mesa-vulkan-drivers vulkan-loader foot dejavu-sans-fonts && dnf clean all
