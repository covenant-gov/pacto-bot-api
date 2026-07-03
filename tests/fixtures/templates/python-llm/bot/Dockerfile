# syntax=docker/dockerfile:1
FROM python:3.14-slim

WORKDIR /app/bots/{{bot_id}}

# Copy the bot package and install it. The bot depends on the pacto-bot-sdk
# package from PyPI, which is resolved automatically by pip.
COPY bots/{{bot_id}}/ ./
RUN pip install --no-cache-dir .

CMD ["python", "{{bot_id_snake}}.py"]
