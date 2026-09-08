ARG RELEASE_TOOL_IMAGE=zhsh-release-toolchain:latest
FROM ${RELEASE_TOOL_IMAGE}

USER root
COPY --chown=release:release source /workspace
RUN mkdir -p /artifacts && chown release:release /artifacts

USER release
WORKDIR /workspace
