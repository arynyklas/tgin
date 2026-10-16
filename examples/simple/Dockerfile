FROM python:3.14-slim

WORKDIR /app

COPY --from=ghcr.io/astral-sh/uv:0.13.1 /uv /uvx /bin/

COPY pyproject.toml uv.lock* ./

RUN uv sync

COPY . .

# bot1; docker-compose.yml also runs bot2 (bot1.py with another reply) and bot3.py from this image.
CMD ["uv", "run", "bot1.py"]
