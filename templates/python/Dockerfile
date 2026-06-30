# syntax=docker/dockerfile:1
FROM python:3.14-slim

WORKDIR /app/bots/{{bot_id}}

# Install the vendored SDK wheel from the project root, then the bot itself.
COPY sdk/ /app/sdk/
COPY bots/{{bot_id}}/ ./
RUN pip install --no-cache-dir /app/sdk/dist/*.whl
RUN pip install --no-cache-dir .

CMD ["python", "{{bot_id_snake}}.py"]
