FROM python:3.14-slim

WORKDIR /app

COPY --from=ghcr.io/astral-sh/uv:0.13.1 /uv /uvx /bin/

COPY pyproject.toml uv.lock* ./

RUN uv sync

COPY . .


COPY bot1.py /app/main.py

CMD ["uv", "run", "main.py"]