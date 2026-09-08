FROM ubuntu:24.04@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517

ARG DEBIAN_FRONTEND=noninteractive
ENV TZ=UTC LC_ALL=C.UTF-8 LANG=C.UTF-8

COPY package.deb /input/package.deb
COPY mock-llm.py /input/mock-llm.py
COPY run-deb-smoke.sh /input/run-deb-smoke.sh
COPY smoke-dpkg.cfg /etc/dpkg/dpkg.cfg.d/zz-zhsh-smoke

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        /input/package.deb \
        procps \
        python3 \
        util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 --shell /bin/bash zhsmoke \
    && chmod 0755 /input/run-deb-smoke.sh /input/mock-llm.py

CMD ["/input/run-deb-smoke.sh"]
