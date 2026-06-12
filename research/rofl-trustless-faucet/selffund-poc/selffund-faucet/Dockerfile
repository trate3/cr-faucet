FROM python:3.12-slim
RUN pip install --no-cache-dir cbor2==5.6.4
COPY selffund.py /app/selffund.py
WORKDIR /app
ENTRYPOINT ["python3", "/app/selffund.py"]
