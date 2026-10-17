# syntax=docker/dockerfile:1

# The grammY and Telegraf bots of the end-to-end suite; the compose service
# picks the script and its mode.

# node:24-slim, Node v24.22.0 (multi-arch index digest)
FROM node:24-slim@sha256:a5f4366e834695d5f363b920c104953a535d3a9864d1bdf74566a0ebe01daee5

WORKDIR /app
ENV NODE_ENV=production

COPY package.json package-lock.json ./
RUN npm ci --omit=dev --ignore-scripts --no-audit --no-fund

COPY grammy_bot.mjs telegraf_bot.mjs ./

USER node
