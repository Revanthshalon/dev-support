# Convenience targets for the local Memgraph stack. Each one is a thin wrapper over
# docker compose, so the README's plain commands keep working without make.
.PHONY: env up up-embeddings up-gpu warmup down logs

# Create .env from the example; never overwrites an existing one.
env:
	@if [ -f .env ]; then echo ".env already exists, left as is"; else cp .env.example .env && echo "created .env: set MEMGRAPH_USER / MEMGRAPH_PASSWORD"; fi

up: env
	docker compose up -d

up-embeddings: env
	docker compose --profile embeddings up -d memgraph lab
	$(MAKE) warmup

up-gpu: env
	docker compose -f docker-compose.yml -f docker-compose.gpu.yml up -d

# Loads EMBEDDINGS_MODEL now; exits non-zero (and so does make) if the model or key is bad.
warmup:
	docker compose --profile embeddings run --rm embeddings-warmup

down:
	docker compose down

logs:
	docker compose logs -f memgraph
