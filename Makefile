# Convenience targets for the local Memgraph stack. Each one is a thin wrapper over
# docker compose, so the README's plain commands keep working without make.
.PHONY: env up up-gpu down logs

# Create .env from the example; never overwrites an existing one.
env:
	@if [ -f .env ]; then echo ".env already exists, left as is"; else cp .env.example .env && echo "created .env: set MEMGRAPH_USER / MEMGRAPH_PASSWORD"; fi

up: env
	docker compose up -d

up-gpu: env
	docker compose -f docker-compose.yml -f docker-compose.gpu.yml up -d

down:
	docker compose down

logs:
	docker compose logs -f memgraph
