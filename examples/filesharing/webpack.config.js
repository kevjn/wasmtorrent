const path = require('path');

module.exports = {
    entry: './index.js',
    output: {
        path: path.resolve(__dirname, 'dist'),
        filename: 'index.js',
        library: {
            type: "module"
        }
    },
    mode: 'development',
    experiments: {
        asyncWebAssembly: true,
        outputModule: true,
   }
};
