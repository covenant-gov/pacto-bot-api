# syntax=docker/dockerfile:1
FROM python:3.14-slim

WORKDIR /app

COPY . .
RUN pip install --no-cache-dir .

CMD ["python", "{{bot_id_snake}}.py"]
