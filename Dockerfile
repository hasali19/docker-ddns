FROM alpine:latest

ARG TARGETPLATFORM

RUN apk add --no-cache openssh

WORKDIR /app

COPY artifacts/$TARGETPLATFORM/docker-ddns /usr/local/bin/docker-ddns
RUN chmod +x /usr/local/bin/docker-ddns

ENTRYPOINT ["/usr/local/bin/docker-ddns"]
