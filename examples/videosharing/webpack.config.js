const path = require('path');

module.exports = {
    entry: './index.jsx',
    output: {
        path: path.resolve(__dirname, 'dist'),
        filename: 'index.js',
        library: {
            type: "module"
        }
    },
    devServer: {
        headers: {
            // required to dynamically import package via url
            "Access-Control-Allow-Origin": "*"
        }
    },
    resolve: {
        extensions: ['.js', '.jsx']
    },
    module: {
        rules: [
            { test: /\.jsx?$/, use: 'babel-loader', exclude: /node_modules/ }
        ]
    },
    mode: 'development',
    experiments: {
        asyncWebAssembly: true,
        outputModule: true,
    }
};
