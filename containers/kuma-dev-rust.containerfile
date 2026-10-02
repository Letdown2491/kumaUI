FROM localhost/kuma-dev-gcc
RUN dnf -y install pkgconf-pkg-config fontconfig-devel freetype-devel libxkbcommon-devel pam-devel && dnf clean all
