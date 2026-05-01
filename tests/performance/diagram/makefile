.PHONY: check render

check:
	uv run ruff format main.py
	uv run ruff check main.py --fix
	uv run ty check main.py

render:
	uv run main.py
