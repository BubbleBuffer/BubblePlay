# Build from the repository root: docker build -f relay/Dockerfile -t bubbleplay-relay .
FROM golang:1.27.1-bookworm AS build
WORKDIR /src
COPY relay/go.mod relay/go.sum ./
RUN go mod download && go mod verify
COPY relay/ ./
RUN CGO_ENABLED=0 GOOS=linux go build -trimpath -ldflags="-s -w" -o /out/bubbleplay-relay . \
    && mkdir -p /out/data /out/licenses/ca-certificates /out/licenses/common-licenses \
    && chmod 700 /out/data \
    && cp /usr/share/doc/ca-certificates/copyright /out/licenses/ca-certificates/copyright \
    && cp -a /usr/share/common-licenses/. /out/licenses/common-licenses/ \
    && dpkg-query -W ca-certificates > /out/licenses/ca-certificates/version.txt

FROM scratch
COPY --from=build /out/bubbleplay-relay /bubbleplay-relay
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /out/licenses /licenses
COPY --from=build --chown=10001:10001 /out/data /data
COPY relay/THIRD-PARTY-NOTICES.md /licenses/THIRD-PARTY-NOTICES.md
COPY LICENSE /licenses/BubblePlay-LICENSE
COPY worker/public/ /assets/
USER 10001:10001
VOLUME ["/data"]
EXPOSE 8787
ENTRYPOINT ["/bubbleplay-relay"]
CMD ["--config", "/data/config.json"]
