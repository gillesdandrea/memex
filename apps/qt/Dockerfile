FROM fedora:44
COPY scripts/install-fedora-deps.sh /tmp/install-fedora-deps.sh
RUN bash /tmp/install-fedora-deps.sh && dnf clean all && rm /tmp/install-fedora-deps.sh
ENV PATH="/usr/lib64/qt6/bin:${PATH}" QT_QPA_PLATFORM=offscreen QT_QUICK_BACKEND=software
WORKDIR /workspace/apps/qt
