# syntax=docker/dockerfile:1

# The aiogram and python-telegram-bot bots of the end-to-end suite; the
# compose service picks the script and its mode.

# ghcr.io/astral-sh/uv:0.9.5 (multi-arch index digest)
FROM ghcr.io/astral-sh/uv:0.9.5@sha256:f459f6f73a8c4ef5d69f4e6fbbdb8af751d6fa40ec34b39a1ab469acd6e289b7 AS uv

# python:3.13-slim, Python 3.13.16 (multi-arch index digest)
FROM python:3.13-slim@sha256:70729b46c69b4f1e97c4822c1af3df53a1476cf5ddc6c087c0c10bc3a5678c2f

COPY --from=uv /uv /usr/local/bin/uv

WORKDIR /app
ENV UV_COMPILE_BYTECODE=1 \
    UV_LINK_MODE=copy \
    PYTHONUNBUFFERED=1 \
    PATH=/app/.venv/bin:$PATH

COPY pyproject.toml uv.lock ./
RUN uv sync --frozen --no-dev

COPY aiogram_bot.py ptb_bot.py ./

USER 65532:65532
