FROM localhost/kuma-dev-gcc
RUN dnf -y install pkgconf-pkg-config fontconfig-devel freetype-devel libxkbcommon-devel pam-devel \
  libavcodec-free-devel libavformat-free-devel libavutil-free-devel libswscale-free-devel clang-devel \
  && dnf clean all
