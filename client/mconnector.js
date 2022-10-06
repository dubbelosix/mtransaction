const fs = require('fs');
const web3 = require('@solana/web3.js')

const path = require("path");
const PROTO_PATH = path.join(__dirname, '..', 'proto', 'mtransaction.proto');

const protoLoader = require('@grpc/proto-loader');
const packageDefinition = protoLoader.loadSync(PROTO_PATH, {keepCase: true});

const grpc = require('@grpc/grpc-js');
const validatorProto = grpc.loadPackageDefinition(packageDefinition).validator;

const getEnvironmentVariable = (key) => {
  const val = process.env[key]
  if (val === undefined) {
    throw new Error(`Environment variable ${key} must be defined!`)
  }
  return val
}

process.on('uncaughtException', (err) => {
    console.log('Caught exception: ' + err + err.stack)
})

function restart(millisecondsToWait, r) {
    console.log(r, "Stream ended", millisecondsToWait);
    setTimeout(() => {
        connect(millisecondsToWait);
    }, millisecondsToWait);
}

class Metrics {
    tx_received = 0
}

function connect(millisecondsToWait) {
    const r = Math.random().toString(36).slice(2);
    const ssl_creds = grpc.credentials.createSsl(
        fs.readFileSync(getEnvironmentVariable('TLS_GRPC_SERVER_CERT')),
        fs.readFileSync(getEnvironmentVariable('TLS_GRPC_CLIENT_KEY')),
        fs.readFileSync(getEnvironmentVariable('TLS_GRPC_CLIENT_CERT')),
    );
    const cluster = new web3.Connection(getEnvironmentVariable('SOLANA_CLUSTER_URL'))
    const metrics = new Metrics()
    const mtransactionClient = new validatorProto.MTransaction(getEnvironmentVariable('GRPC_SERVER_ADDR'), ssl_creds);

    const call = mtransactionClient.TxStream({ message: 'Listening for messages' }, (err, message) => {
        console.log(r, err, message);
    });

    const sendPong = (id) => call.write({ pong: { id } })
    const sendMetrics = () => call.write({ metrics })

    const processTx = (r, { data }) => {
        console.log(r, 'tx', data)
        try {
            // cluster.sendRawTransaction(Buffer.from(data, 'base64'), { preflightCommitment: 'processed' }).then((v) => console.log(r, 'tx', v))
        } catch (err) {
            console.log(r, data, err)
        }
    }
    const processPing = (r, { id }) => {
        console.log(r, 'ping', id)
        sendPong(id)
    }

    const scheduleMetrics = () => setTimeout(() => {
        try {
            if (!call.writesClosed) {
                sendMetrics()
                scheduleMetrics()
            }
        } catch (err) {
            console.log('Failed to send metrics', r, err)
        }
    }, 15000)
    scheduleMetrics()

    call.on('data', ({ tx, ping }) => {
        if (tx) {
            processTx(r, tx)
        }
        if (ping) {
            processPing(r, ping)
        }
        millisecondsToWait = 500;
    });
    call.on('end', () => {
        millisecondsToWait += millisecondsToWait;
        mtransactionClient.close();
        console.log(r, "connection end")
        restart(millisecondsToWait, r);
    });
    call.on('error', (err) => {
        console.error(r, err);
    });
}

function main() {
    connect(500);
}

main();
