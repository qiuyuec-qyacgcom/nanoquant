# ---- 前端构建 ----
# 用 npm ci（可复现）而非 npm install；基础镜像锁到具体 minor，避免"哪天 latest 变了"
FROM node:20.18-alpine AS frontend
WORKDIR /build
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY frontend/ ./
RUN npm run build

# ---- 运行时 ----
FROM python:3.12.7-slim
ENV PYTHONUNBUFFERED=1 \
    NANOQUANT_HOME=/app \
    NANOQUANT_PORT=8899
WORKDIR /app

# 非 root 运行：容器内最小权限，万一应用被攻破不能直接读写宿主
RUN useradd --create-home --uid 1000 nanoquant

COPY backend/requirements.txt /app/backend/requirements.txt
RUN pip install --no-cache-dir -r /app/backend/requirements.txt
COPY backend/ /app/backend/
COPY --from=frontend /build/dist /app/frontend/dist

RUN mkdir -p /app/data /app/logs && chown -R nanoquant:nanoquant /app
USER nanoquant

EXPOSE 8899
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s \
  CMD python -c "import urllib.request,sys; sys.exit(0 if urllib.request.urlopen('http://127.0.0.1:8899/api/v1/health', timeout=4).status==200 else 1)"
WORKDIR /app/backend
CMD ["python", "-m", "nanoquant"]
