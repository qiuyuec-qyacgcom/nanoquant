FROM node:20-alpine AS frontend
WORKDIR /build
COPY frontend/package*.json ./
RUN npm install --no-audit --no-fund
COPY frontend/ ./
RUN npm run build

FROM python:3.12-slim
ENV PYTHONUNBUFFERED=1 \
    NANOQUANT_HOME=/app \
    NANOQUANT_PORT=8899
WORKDIR /app
COPY backend/requirements.txt /app/backend/requirements.txt
RUN pip install --no-cache-dir -r /app/backend/requirements.txt
COPY backend/ /app/backend/
COPY --from=frontend /build/dist /app/frontend/dist
RUN mkdir -p /app/data /app/logs
EXPOSE 8899
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s \
  CMD python -c "import urllib.request,sys; sys.exit(0 if urllib.request.urlopen('http://127.0.0.1:8899/api/v1/health', timeout=4).status==200 else 1)"
WORKDIR /app/backend
CMD ["python", "-m", "nanoquant"]
