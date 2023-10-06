if (process.env.JEST_TIMEOUT)
    jest.setTimeout(Number(process.env.JEST_TIMEOUT)); // in milliseconds
