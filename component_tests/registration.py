#!/usr/bin/env python3
import logging

import asyncio


from aiocoap import *

import asyncio
import pytest

pytest_plugins = ('pytest_asyncio',)


logging.basicConfig(level=logging.INFO)

logger = logging.getLogger(__name__)

@pytest.mark.asyncio
async def test_registration():

    protocol = await Context.create_client_context()

    payload = '</>;rt="oma.lwm2m";ct=112,</1/1>,</3>;ver=1.0,</3/0>,</5>;ver=1.0,</5/0>'
    request = Message(code=PUT, uri="coap://localhost/rd", payload=payload.encode())
    request.remote.maximum_block_size_exp = 0
    try:
        response = await protocol.request(request, handle_blockwise=True).response

    except Exception as e:
        logger.error("Failed to register", exc_info=e)
        raise
    else:
        logger.info("Result: %s\n%r" % (response.code, response.payload))
