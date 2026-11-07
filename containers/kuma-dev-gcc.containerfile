# The build chain's root layer. Tracks the HOST's Fedora release (the
# kumaOS the binaries install onto), not latest and not a frozen pin:
# the binaries land in ~/.local/bin and must run there, and libav's
# sonames move between Fedora releases (the v0.8.0 cut's stamp assert
# caught a kuma-files built against the 44 container's libav the 45
# host no longer shipped). When the kumaOS image moves its Fedora
# base, move this line with it, then rebuild both images:
#
#   podman build -t localhost/kuma-dev-gcc -f containers/kuma-dev-gcc.containerfile containers/
#   podman build -t localhost/kuma-dev-rust -f containers/kuma-dev-rust.containerfile containers/
#
# The cargo caches are run-time named volumes (see build.sh), so the
# rebuilds are cheap and the first build after re-derives only toolchains.
FROM registry.fedoraproject.org/fedora-toolbox:45
RUN dnf -y install gcc musl-gcc && dnf clean all
