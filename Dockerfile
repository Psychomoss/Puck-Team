FROM debian:bookworm-slim AS build
RUN apt-get update && apt-get install -y --no-install-recommends g++ libzstd-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY build.sh ./
COPY src src
RUN sh build.sh /usr/local/bin/sessions && strip /usr/local/bin/sessions

FROM debian:bookworm-slim
COPY --from=build /usr/local/bin/sessions /usr/local/bin/sessions
# glibc malloc backs its large allocations with transparent huge pages: far fewer page faults.
ENV GLIBC_TUNABLES=glibc.malloc.hugetlb=1
USER 1000:1000
CMD ["/usr/local/bin/sessions"]
